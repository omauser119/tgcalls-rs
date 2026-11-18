//! Panic-contained synchronous primitives; the Python API runs them via asyncio.to_thread.
use anyhow::{Result, anyhow, bail};
use pyo3::{
    exceptions::{PyRuntimeError, PyTimeoutError, PyValueError},
    prelude::*,
    types::PyBytes,
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tgcalls_core::{AudioFrame, Impairment, session::Session};
use tgcalls_sandbox::{Config, Sandbox};

macro_rules! ffi_guard {
    ($body:expr) => {{
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)).unwrap_or_else(|_| {
            Err(PyRuntimeError::new_err(
                "Rust panic contained at FFI boundary",
            ))
        })
    }};
}

#[pyclass(frozen)]
struct PersonalWorker(Mutex<tgcalls_core::worker::Worker>);

#[pymethods]
impl PersonalWorker {
    #[new]
    fn new() -> PyResult<Self> {
        ffi_guard!(Ok(Self(Mutex::new(Default::default()))))
    }

    fn request(&self, py: Python<'_>, input: Vec<u8>) -> PyResult<Py<PyBytes>> {
        ffi_guard!({
            let reply = py.detach(|| {
                self.0
                    .lock()
                    .map_err(|_| PyRuntimeError::new_err("personal call worker failed"))
                    .map(|mut worker| worker.execute(&input))
            })?;
            Ok(PyBytes::new(py, &reply).unbind())
        })
    }
}

enum Backend {
    Standard(Arc<Session>),
    Hardened(Arc<Sandbox>),
}

#[pyclass(frozen)]
struct Engine {
    runtime: tokio::runtime::Runtime,
    backend: Mutex<Option<Arc<Backend>>>,
    hardened: bool,
    helper: Option<PathBuf>,
    bind_ip: Option<IpAddr>,
    lifecycle: Mutex<()>,
    closed: AtomicBool,
}

fn python_error(error: anyhow::Error) -> PyErr {
    if error
        .downcast_ref::<tgcalls_core::Error>()
        .is_some_and(|e| matches!(e, tgcalls_core::Error::Invalid(_)))
    {
        return PyValueError::new_err(error.to_string());
    }
    if error
        .downcast_ref::<tgcalls_core::Error>()
        .is_some_and(|e| matches!(e, tgcalls_core::Error::Timeout))
        || error.to_string().contains("timed out")
    {
        PyTimeoutError::new_err(error.to_string())
    } else {
        PyRuntimeError::new_err(format!("{error:#}"))
    }
}

impl Engine {
    fn current(&self) -> Result<Arc<Backend>> {
        self.backend
            .lock()
            .map_err(|_| anyhow!("engine lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow!("engine not started"))
    }
    fn install(&self, backend: Backend) -> Result<()> {
        let mut current = self
            .backend
            .lock()
            .map_err(|_| anyhow!("engine lock poisoned"))?;
        if current.is_some() {
            bail!("engine already started");
        }
        *current = Some(Arc::new(backend));
        Ok(())
    }
    fn check_startable(&self) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            bail!("engine closed");
        }
        if self
            .backend
            .lock()
            .map_err(|_| anyhow!("engine lock poisoned"))?
            .is_some()
        {
            bail!("engine already started");
        }
        Ok(())
    }
    fn local_ip(&self, loopback: bool) -> Result<IpAddr> {
        if let Some(ip) = self.bind_ip {
            return Ok(ip);
        }
        if loopback {
            return Ok(Ipv4Addr::LOCALHOST.into());
        }
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        // UDP connect selects a source address from the routing table; it sends no datagram.
        socket.connect(SocketAddr::new(
            Ipv4Addr::new(149, 154, 167, 50).into(),
            443,
        ))?;
        Ok(socket.local_addr()?.ip())
    }
    fn helper_path(&self) -> Result<PathBuf> {
        let path = self
            .helper
            .clone()
            .or_else(|| std::env::var_os("TGCALLS_SANDBOX_BIN").map(PathBuf::from))
            .ok_or_else(|| anyhow!("hardened mode requires helper_path or TGCALLS_SANDBOX_BIN"))?;
        // No PATH lookup: require a concrete helper binary selected by the application.
        if !path.is_absolute() {
            bail!("sandbox helper path must be absolute");
        }
        Ok(path)
    }
}

#[pymethods]
impl Engine {
    #[new]
    #[pyo3(signature = (hardened=false, helper_path=None, bind_ip=None))]
    fn new(
        hardened: bool,
        helper_path: Option<PathBuf>,
        bind_ip: Option<String>,
    ) -> PyResult<Self> {
        ffi_guard!({
            let bind_ip = bind_ip
                .map(|ip| {
                    ip.parse()
                        .map_err(|_| PyValueError::new_err("invalid bind IP"))
                })
                .transpose()?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            Ok(Self {
                runtime,
                backend: Mutex::new(None),
                hardened,
                helper: helper_path,
                bind_ip,
                lifecycle: Mutex::new(()),
                closed: AtomicBool::new(false),
            })
        })
    }

    #[pyo3(signature = (loss_percent=0.0, jitter_ms=0, delay_ms=0, seed=1))]
    fn start_loopback(
        &self,
        py: Python<'_>,
        loss_percent: f64,
        jitter_ms: u64,
        delay_ms: u64,
        seed: u64,
    ) -> PyResult<()> {
        ffi_guard!(py.detach(|| {
            let _lifecycle = self
                .lifecycle
                .lock()
                .map_err(|_| PyRuntimeError::new_err("lifecycle lock poisoned"))?;
            self.check_startable().map_err(python_error)?;
            let result = self.runtime.block_on(async {
                let ip = self.local_ip(true)?;
                let impairment = Impairment {
                    loss_percent,
                    jitter_ms,
                    delay_ms,
                    seed,
                };
                impairment.validate()?;
                let backend = if self.hardened {
                    Backend::Hardened(
                        Sandbox::start(self.helper_path()?, Config::Loopback { impairment }, ip)
                            .await?,
                    )
                } else {
                    Backend::Standard(Arc::new(
                        Session::loopback(impairment, &ip.to_string(), None).await?,
                    ))
                };
                self.install(backend)
            });
            result.map_err(python_error)
        }))
    }

    fn create_call(&self, py: Python<'_>, ssrc: u32) -> PyResult<String> {
        ffi_guard!(py.detach(|| {
            let _lifecycle = self
                .lifecycle
                .lock()
                .map_err(|_| PyRuntimeError::new_err("lifecycle lock poisoned"))?;
            self.check_startable().map_err(python_error)?;
            self.runtime
                .block_on(async {
                    let ip = self.local_ip(false)?;
                    let backend = if self.hardened {
                        Backend::Hardened(
                            Sandbox::start(self.helper_path()?, Config::Telegram { ssrc }, ip)
                                .await?,
                        )
                    } else {
                        Backend::Standard(Arc::new(
                            Session::group(&ip.to_string(), None, ssrc).await?,
                        ))
                    };
                    let payload = match &backend {
                        Backend::Standard(session) => session.join_payload().await?,
                        Backend::Hardened(sandbox) => sandbox.join_payload().await?,
                    };
                    self.install(backend)?;
                    Ok::<_, anyhow::Error>(payload)
                })
                .map_err(python_error)
        }))
    }

    fn join_payload(&self, py: Python<'_>) -> PyResult<String> {
        ffi_guard!(py.detach(|| {
            self.runtime
                .block_on(async {
                    match &*self.current()? {
                        Backend::Standard(session) => Ok(session.join_payload().await?),
                        Backend::Hardened(sandbox) => sandbox.join_payload().await,
                    }
                })
                .map_err(python_error)
        }))
    }

    fn connect(&self, py: Python<'_>, params: String) -> PyResult<()> {
        ffi_guard!(py.detach(|| {
            self.runtime
                .block_on(async {
                    match &*self.current()? {
                        Backend::Standard(session) => Ok(session.connect_telegram(&params).await?),
                        Backend::Hardened(sandbox) => sandbox.connect(params).await,
                    }
                })
                .map_err(python_error)
        }))
    }

    fn set_sources(&self, py: Python<'_>, ssrcs: Vec<u32>) -> PyResult<()> {
        ffi_guard!(py.detach(|| {
            self.runtime
                .block_on(async {
                    match &*self.current()? {
                        Backend::Standard(session) => Ok(session.set_sources(ssrcs).await?),
                        Backend::Hardened(sandbox) => sandbox.set_sources(ssrcs).await,
                    }
                })
                .map_err(python_error)
        }))
    }

    fn send_pcm(&self, py: Python<'_>, pcm: Vec<u8>) -> PyResult<u32> {
        ffi_guard!(py.detach(|| {
            self.runtime
                .block_on(async {
                    match &*self.current()? {
                        Backend::Standard(session) => Ok(session.send_pcm(&pcm).await?),
                        Backend::Hardened(sandbox) => sandbox.send_pcm(pcm).await,
                    }
                })
                .map_err(python_error)
        }))
    }

    #[pyo3(signature = (timeout_ms=1000))]
    #[allow(clippy::type_complexity)]
    fn receive(
        &self,
        py: Python<'_>,
        timeout_ms: u64,
    ) -> PyResult<(u32, u16, u32, bool, u32, Py<PyBytes>)> {
        ffi_guard!({
            if timeout_ms > 60_000 {
                return Err(PyValueError::new_err("timeout exceeds 60 seconds"));
            }
            let frame: AudioFrame = py.detach(|| {
                self.runtime
                    .block_on(async {
                        match &*self.current()? {
                            Backend::Standard(session) => {
                                Ok(session.receive(Duration::from_millis(timeout_ms)).await?)
                            }
                            Backend::Hardened(sandbox) => {
                                sandbox.receive(Duration::from_millis(timeout_ms)).await
                            }
                        }
                    })
                    .map_err(python_error)
            })?;
            Ok((
                frame.ssrc,
                frame.sequence,
                frame.timestamp,
                frame.concealed,
                frame.target_delay_ms,
                PyBytes::new(py, &frame.pcm).unbind(),
            ))
        })
    }

    fn stats_json(&self, py: Python<'_>) -> PyResult<String> {
        ffi_guard!(py.detach(|| self.runtime.block_on(async {
            let (stats, generation, needs_rejoin, pid, last_error) = match &*self.current()? {
                Backend::Standard(session) => (session.stats(), 0, false, None, None),
                Backend::Hardened(sandbox) => (sandbox.stats().await?, sandbox.generation(), sandbox.needs_rejoin(), sandbox.worker_pid().await, sandbox.last_error().await),
            };
            Ok::<_, anyhow::Error>(serde_json::json!({"media": stats, "hardened": self.hardened, "generation": generation,
                "needs_rejoin": needs_rejoin, "worker_pid": pid, "last_error": last_error}).to_string())
        }).map_err(python_error)))
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        ffi_guard!(py.detach(|| {
            let _lifecycle = self
                .lifecycle
                .lock()
                .map_err(|_| PyRuntimeError::new_err("lifecycle lock poisoned"))?;
            self.closed.store(true, Ordering::Release);
            let backend = self
                .backend
                .lock()
                .map_err(|_| PyRuntimeError::new_err("engine lock poisoned"))?
                .take();
            if let Some(backend) = backend {
                self.runtime.block_on(async {
                    match &*backend {
                        Backend::Standard(session) => session
                            .close()
                            .await
                            .map_err(|e| PyRuntimeError::new_err(e.to_string())),
                        Backend::Hardened(sandbox) => {
                            sandbox.close().await;
                            Ok(())
                        }
                    }
                })?;
            }
            Ok(())
        }))
    }
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    ffi_guard!({
        module.add_class::<Engine>()?;
        module.add_class::<PersonalWorker>()?;
        module.add("PCM_BYTES", tgcalls_core::PCM_BYTES)?;
        Ok(())
    })
}
