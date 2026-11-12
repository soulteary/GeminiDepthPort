"""Read-only preview of the service's startup report; never opens an SDK device."""
import hashlib
import json
from pathlib import Path


def startup_report(config=Path('/config/capture.json'), status=Path('/tmp/capture/status.json')):
    if not config.exists():
        return None
    try:
        with config.open('rb') as source:
            raw_config = source.read(65537)
        with status.open('rb') as source:
            raw = source.read(512 * 1024 + 1)
        if len(raw_config) > 65536 or len(raw) > 512 * 1024:
            raise ValueError('device report byte limit')
        value = json.loads(raw)
        if value.get('config_sha256') != hashlib.sha256(raw_config).hexdigest():
            raise ValueError('device report configuration differs')
        report = value.get('device_capabilities')
        if report is None:
            return {'available': False, 'error': '服务尚未提供设备启动报告'}
        if not isinstance(report, dict) or report.get('version') != 1:
            raise ValueError('device report schema')
        if len(json.dumps(report).encode()) > 65536:
            raise ValueError('device report byte limit')
        return report
    except (OSError, ValueError, TypeError, AttributeError) as error:
        return {'available': False, 'error': str(error)[:256]}
