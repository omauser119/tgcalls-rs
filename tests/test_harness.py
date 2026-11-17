"""Static/pure harness checks only. No benchmark scenario is executed."""
import importlib.util
from pathlib import Path
import subprocess
import sys


def test_list_is_safe_without_loading_native_module():
    result = subprocess.run([sys.executable, "benchmarks/run.py", "--list"],
                            capture_output=True, text=True, check=True)
    assert '"loss-jitter"' in result.stdout


def test_metrics_math_and_pcm_format():
    path = Path("benchmarks/run.py")
    spec = importlib.util.spec_from_file_location("benchmark_definitions", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    assert module.percentile([0, 10, 20], 95) == 19
    assert module.percentile([], 95) is None
    assert len(module.tone()) == 3840
