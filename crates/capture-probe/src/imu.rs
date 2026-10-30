use crate::rgbd::{Config, Metrics};
use anyhow::{Result, ensure};
use capture_contract::imu::ImuSample;
use serde_json::json;
use std::time::Instant;
#[derive(Default)]
pub(crate) struct ImuMetrics {
    received: [u64; 2],
    published: [u64; 2],
    excluded: u64,
    peak: usize,
    wall: capture_contract::timing::StageStats,
    error: Option<String>,
    ros: bool,
}
impl ImuMetrics {
    pub fn ready(&self) -> bool {
        self.received.iter().all(|v| *v > 0)
            && (!self.ros || self.published.iter().all(|v| *v > 0))
            && self.error.is_none()
    }
    pub fn report(&self) -> serde_json::Value {
        json!({"enabled":true,"received":self.received,"published":self.published,"startup_samples_excluded":self.excluded,"queue_high_water":self.peak,"error":self.error,"ready":self.ready(),"pump_wall":self.wall.report(),"units":{"accel":"m/s^2","gyro":"rad/s"},"orientation_available":false,"extrinsics_verified":false,"interpolation":false,"delivery_verified":false})
    }
}
pub(crate) struct RuntimeImu {
    capture: orbbec_safe::ImuCapture,
    publisher: Option<ros_output::ImuPublisher>,
}
impl RuntimeImu {
    pub fn open(config: &Config, pipeline: &orbbec_safe::Pipeline) -> Result<Option<Self>> {
        let Some(imu) = config.imu.as_ref().filter(|v| v.enabled) else {
            return Ok(None);
        };
        let global = config.message_timestamp() == capture_contract::MessageTimestamp::Global;
        // Create the ROS context before callbacks begin, avoiding a startup queue
        // burst while discovery/publisher allocation is still initializing.
        let publisher = config
            .ros
            .as_ref()
            .map(|r| ros_output::ImuPublisher::load(&r.bridge, &r.namespace, imu.clone(), global))
            .transpose()?;
        Ok(Some(Self {
            capture: pipeline.start_imu(imu.clone(), global)?,
            publisher,
        }))
    }
    pub fn tick(&mut self, metrics: &mut Metrics, publish: bool) -> Result<Vec<ImuSample>> {
        let stats = metrics
            .imu
            .get_or_insert_with(|| Box::new(ImuMetrics::default()));
        let started = Instant::now();
        stats.ros = self.publisher.is_some();
        let (received, peak) = self.capture.counters();
        stats.received = received;
        stats.peak = peak;
        let result = (|| -> Result<Vec<ImuSample>> {
            let samples = self.capture.drain()?;
            if publish {
                if let Some(publisher) = &mut self.publisher {
                    for sample in &samples {
                        publisher.publish(sample)?;
                        stats.published[sample.kind.slot()] += 1;
                    }
                }
            } else {
                stats.excluded += samples.len() as u64;
            }
            Ok(if publish { samples } else { Vec::new() })
        })();
        stats.wall.observe(started.elapsed());
        if let Err(e) = &result {
            stats.error = Some(e.to_string());
        }
        result
    }
    pub fn stop(&mut self, metrics: &mut Metrics, publish: bool) -> Result<Vec<ImuSample>> {
        self.capture.stop()?;
        let tail = self.tick(metrics, publish)?;
        ensure!(
            metrics
                .imu
                .as_ref()
                .is_some_and(|s| s.received.iter().all(|v| *v > 0)),
            "IMU streams produced no samples"
        );
        Ok(tail)
    }
}
