#!/usr/bin/env python3
"""Real DDS wire + asynchronous diagnostic writes, synthetic images only."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def require(value, message):
    if not value:
        raise AssertionError(message)


def main():
    with tempfile.TemporaryDirectory(prefix='gdp-diagnostics-smoke-') as folder:
        root = Path(folder)
        environment = dict(os.environ, PYTHONPATH='/ws' + os.pathsep + os.environ.get('PYTHONPATH', ''))
        with (root/'diagnostics.json').open('w') as output, (root/'diagnostics.err').open('w') as errors:
            child = subprocess.Popen([sys.executable, '-m', 'runtime.pipeline_diagnostics',
                '--namespace', '/gdp_pipeline_diagnostics_smoke', '--seconds', '14',
                '--queue-capacity', '128', '--queue-bytes', '1048576', '--sync-every', '10',
                '--output-dir', str(root/'raw')], env=environment, stdout=output, stderr=errors)
            try:
                # An explicit synthetic discovery interval, not an acquisition fix.
                time.sleep(1)
                producer = subprocess.run([sys.executable, '/ws/ros/tests/smoke_ros.py',
                    '--library', '/opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so',
                    '--namespace', '/gdp_pipeline_diagnostics_smoke', '--stamp-source', 'global',
                    '--observed-timing'], capture_output=True, text=True, timeout=30)
                require(producer.returncode == 0, producer.stdout + producer.stderr)
                require(child.wait(timeout=30) == 0, (root/'diagnostics.err').read_text() + (root/'diagnostics.json').read_text())
            finally:
                if child.poll() is None:
                    child.kill();child.wait()
        summary = json.loads((root/'diagnostics.json').read_text())
        for role in ('color', 'depth'):
            stream = summary['streams'][role]
            require(stream['images'] >= 5 and stream['joined'] >= 5, str(stream))
            for stage in ('sdk_system_to_return', 'sdk_return_to_publication_start',
                          'publication_start_to_subscriber', 'ros_image_publish_wall', 'subscriber_callback'):
                require(stream['timing'][stage]['count'] >= 5, role + ':' + stage)
        writer = summary['writer']
        require(writer['completed'] is True and writer['error'] is None, str(writer))
        require(writer['written_images'] == sum(summary['streams'][role]['images'] for role in ('color','depth')), 'recorded image count differs')
        require(writer['fsynced_payloads'] == writer['written_images'], 'payload sync incomplete')
        require(writer['queue']['depth'] == 0 and writer['queue']['in_flight'] == 0, 'writer backlog remains')
        require(summary['host_pressure']['samples'] >= 2, 'pressure sampler did not run')
        print(json.dumps({'ok': True, 'kind':'synthetic-stage-diagnostics-wire-and-disk',
            'images_written':writer['written_images'], 'metadata_written':writer['written_metadata'],
            'all_stage_timings_observed':True, 'queue_drained':True, 'final_sync_completed':True,
            'hardware_verified':False, 'original_355ms_tail_fixed':False}))


if __name__ == '__main__':
    main()
