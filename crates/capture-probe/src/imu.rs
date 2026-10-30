use crate::rgbd::{Config, Metrics};
use anyhow::{Result, ensure};
use capture_contract::imu::SharedImuSample;
use serde_json::json;
use std::time::Instant;
#[derive(Default)]
pub(crate) struct ImuMetrics {
    received: [u64; 2],
    published: [u64; 2],
    excluded: u64,
    peak: usize,
    wall: capture_contract::timing::StageStats,
    drain_wall: capture_contract::timing::StageStats,
    publish_wall: capture_contract::timing::StageStats,
    error: Option<String>,
    ros: bool,
    shared_context: bool,
}
impl ImuMetrics {
    pub fn ready(&self) -> bool {
        self.received.iter().all(|v| *v > 0)
            && (!self.ros || self.published.iter().all(|v| *v > 0))
            && self.error.is_none()
    }
    pub fn report(&self) -> serde_json::Value {
        json!({"enabled":true,"received":self.received,"published":self.published,"startup_samples_excluded":self.excluded,"queue_high_water":self.peak,"error":self.error,"ready":self.ready(),"pump_wall":self.wall.report(),"drain_wall":self.drain_wall.report(),"ros_publish_wall":self.publish_wall.report(),"timing_scope":{"drain_wall":"per pump, counters and SDK queue drain including lock wait","ros_publish_wall":"per nonempty published batch, all synchronous ROS calls including failures","pump_wall":"whole IMU pump; excludes native recorder buffering"},"units":{"accel":"m/s^2","gyro":"rad/s"},"orientation_available":false,"extrinsics_verified":false,"interpolation":false,"delivery_verified":false,"shared_ros_context":self.shared_context})
    }
}
pub(crate) struct RuntimeImu {
    capture: orbbec_safe::ImuCapture,
    samples: Vec<SharedImuSample>,
    publisher: Option<ros_output::ImuPublisher>,
    context: Option<ros_output::SharedRosContext>,
}
impl RuntimeImu {
    pub fn open(config: &Config, pipeline: &orbbec_safe::Pipeline) -> Result<Option<Self>> {
        let Some(imu) = config.imu.as_ref().filter(|v| v.enabled) else {
            return Ok(None);
        };
        let global = config.message_timestamp() == capture_contract::MessageTimestamp::Global;
        // Create the ROS context before callbacks begin, avoiding a startup queue
        // burst while discovery/publisher allocation is still initializing.
        let context = config
            .ros
            .as_ref()
            .map(|r| ros_output::SharedRosContext::try_load(&r.bridge))
            .transpose()?
            .flatten();
        let publisher = config
            .ros
            .as_ref()
            .map(|r| match &context {
                Some(c) => {
                    ros_output::ImuPublisher::load_shared(c, &r.namespace, imu.clone(), global)
                }
                None => {
                    ros_output::ImuPublisher::load(&r.bridge, &r.namespace, imu.clone(), global)
                }
            })
            .transpose()?;
        Ok(Some(Self {
            capture: pipeline.start_imu(imu.clone(), global)?,
            samples: Vec::with_capacity(imu.queue_capacity),
            publisher,
            context,
        }))
    }
    pub fn context(&self) -> Option<&ros_output::SharedRosContext> {
        self.context.as_ref()
    }
    pub fn tick(
        &mut self,
        metrics: &mut Metrics,
        publish: bool,
    ) -> Result<&mut Vec<SharedImuSample>> {
        let stats = metrics
            .imu
            .get_or_insert_with(|| Box::new(ImuMetrics::default()));
        let started = Instant::now();
        stats.ros = self.publisher.is_some();
        stats.shared_context = self.context.is_some();
        let drain_started = Instant::now();
        let drained = self.capture.drain_into(&mut self.samples);
        stats.drain_wall.observe(drain_started.elapsed());
        let (received, peak) = match &drained {
            Ok(counters) => *counters,
            Err(_) => self.capture.counters(),
        };
        stats.received = received;
        stats.peak = peak;
        let result = (|| -> Result<()> {
            drained?;
            if publish {
                if let Some(publisher) = &mut self.publisher {
                    if !self.samples.is_empty() {
                        let publish_started = Instant::now();
                        let result = (|| -> Result<()> {
                            for sample in &self.samples {
                                publisher.publish_shared(sample)?;
                                stats.published[sample.kind.slot()] += 1;
                            }
                            Ok(())
                        })();
                        stats.publish_wall.observe(publish_started.elapsed());
                        result?;
                    }
                }
            } else {
                stats.excluded += self.samples.len() as u64;
                self.samples.clear();
            }
            Ok(())
        })();
        stats.wall.observe(started.elapsed());
        if let Err(e) = &result {
            stats.error = Some(e.to_string());
        }
        result?;
        Ok(&mut self.samples)
    }
    pub fn stop(
        &mut self,
        metrics: &mut Metrics,
        publish: bool,
    ) -> Result<&mut Vec<SharedImuSample>> {
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
