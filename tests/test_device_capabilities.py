"""Read-only, configuration-bound preview reports; no SDK or camera access."""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('device_capabilities',ROOT/'runtime/device_capabilities.py')
helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(helper)

class StartupReportTests(unittest.TestCase):
    def test_observation_is_bounded_bound_to_actual_config_and_never_opens_devices(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);config=root/'capture.json';status=root/'status.json'
            self.assertIsNone(helper.startup_report(config,status))
            config.write_bytes(b'{"synthetic":true}\n')
            self.assertFalse(helper.startup_report(config,status)['available'])
            report={'version':1,'available':True,'info':{'firmware':'1.4.98'},'properties':[]}
            status.write_text(json.dumps({'config_sha256':hashlib.sha256(config.read_bytes()).hexdigest(),'device_capabilities':report}))
            self.assertEqual(helper.startup_report(config,status),report)
            before=(config.read_bytes(),status.read_bytes())
            helper.startup_report(config,status)
            self.assertEqual(before,(config.read_bytes(),status.read_bytes()))
            config.write_bytes(b'changed')
            self.assertIn('configuration differs',helper.startup_report(config,status)['error'])

    def test_missing_invalid_and_oversized_reports_stay_unknown(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);config=root/'capture.json';status=root/'status.json';config.write_bytes(b'{}')
            digest=hashlib.sha256(b'{}').hexdigest()
            for report in [None,[],{'version':2},{'version':1,'data':'x'*17000}]:
                status.write_text(json.dumps({'config_sha256':digest,'device_capabilities':report}))
                self.assertFalse(helper.startup_report(config,status)['available'])
            for raw in ['{','null',' '* (512*1024+1)]:
                status.write_text(raw);self.assertFalse(helper.startup_report(config,status)['available'])
            config.write_bytes(b' '*65537)
            self.assertIn('byte limit',helper.startup_report(config,status)['error'])

if __name__=='__main__':unittest.main()
