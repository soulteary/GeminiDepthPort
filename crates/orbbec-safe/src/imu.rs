use super::*;
use capture_contract::imu::{ImuConfig, ImuKind, ImuProgress, ImuSample};
use std::{collections::VecDeque, ffi::c_void, sync::Arc, time::Instant};
struct Queue {
    samples: VecDeque<ImuSample>,
    error: Option<&'static str>,
    progress: ImuProgress,
    last: [Instant; 2],
    received: [u64; 2],
    high_water: usize,
}
struct CallbackState {
    queue: Arc<Mutex<Queue>>,
    config: ImuConfig,
    kind: ImuKind,
    imu: sys::ImuApi,
    index: unsafe extern "C" fn(*const sys::Handle, sys::ErrorOut) -> u64,
    device: unsafe extern "C" fn(*const sys::Handle, sys::ErrorOut) -> u64,
    system: unsafe extern "C" fn(*const sys::Handle, sys::ErrorOut) -> u64,
    global: Option<sys::GlobalTimestampFn>,
    delete: unsafe extern "C" fn(*const sys::Handle, sys::ErrorOut),
    delete_error: unsafe extern "C" fn(*mut sys::Error),
}
impl CallbackState {
    unsafe fn read(&self, frame: *mut sys::Handle) -> Option<ImuSample> {
        let mut e = ptr::null_mut();
        macro_rules! call {
            ($expr:expr) => {{
                let v = $expr;
                if !e.is_null() {
                    unsafe { (self.delete_error)(e) };
                    return None;
                }
                v
            }};
        }
        let kind = call!(unsafe { (self.imu.frame_type)(frame, &mut e) });
        if kind != if self.kind == ImuKind::Accel { 4 } else { 7 } {
            return None;
        }
        let value = call!(unsafe {
            (if self.kind == ImuKind::Accel {
                self.imu.accel
            } else {
                self.imu.gyro
            })(frame, &mut e)
        });
        let sequence = call!(unsafe { (self.index)(frame, &mut e) });
        let device_timestamp_us = call!(unsafe { (self.device)(frame, &mut e) });
        let system_timestamp_us = call!(unsafe { (self.system)(frame, &mut e) });
        let global_timestamp_us = if let Some(global) = self.global {
            Some(call!(unsafe { global(frame, &mut e) }))
        } else {
            None
        };
        Some(ImuSample {
            kind: self.kind,
            sequence,
            device_timestamp_us,
            system_timestamp_us,
            global_timestamp_us,
            xyz: [value.x.into(), value.y.into(), value.z.into()],
            frame_id: if self.kind == ImuKind::Accel {
                self.config.accel_frame_id.clone()
            } else {
                self.config.gyro_frame_id.clone()
            },
        })
    }
}
unsafe extern "C" fn callback(frame: *mut sys::Handle, user: *mut c_void) {
    // SDK transfers the callback frame's reference to this function. Never let
    // a Rust panic cross C, and release the reference on every error path.
    let state = unsafe { &*(user.cast::<CallbackState>()) };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let sample = if frame.is_null() {
            None
        } else {
            unsafe { state.read(frame) }
        };
        let mut queue = state.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue.error.is_some() {
            return;
        }
        match sample {
            Some(sample) if queue.samples.len() < state.config.queue_capacity => {
                if queue.progress.accept(&sample, &state.config).is_err() {
                    queue.error = Some("IMU_INVALID_OR_REGRESSING");
                    return;
                }
                let slot = sample.kind.slot();
                queue.last[slot] = Instant::now();
                queue.received[slot] += 1;
                queue.samples.push_back(sample);
                queue.high_water = queue.high_water.max(queue.samples.len());
            }
            Some(_) => queue.error = Some("IMU_QUEUE_OVERFLOW"),
            None => queue.error = Some("IMU_CALLBACK_FAILED"),
        }
    }));
    if !frame.is_null() {
        let mut e = ptr::null_mut();
        unsafe { (state.delete)(frame, &mut e) };
        if !e.is_null() {
            unsafe { (state.delete_error)(e) };
            state.queue.lock().unwrap_or_else(|e| e.into_inner()).error =
                Some("IMU_FRAME_RELEASE_FAILED");
        }
    }
    if outcome.is_err() {
        state.queue.lock().unwrap_or_else(|e| e.into_inner()).error = Some("IMU_CALLBACK_PANIC");
    }
}
struct Sensor {
    handle: Option<Handle>,
    state: Option<Box<CallbackState>>,
    session: Rc<Session>,
    active: bool,
    retain: bool,
    registration: Option<Registration>,
    api: sys::ImuApi,
}
impl Sensor {
    fn stop(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let mut e = ptr::null_mut();
        unsafe { (self.api.stop)(self.handle.as_ref().unwrap().raw(), &mut e) };
        check(self.session.api(), e)?;
        self.active = false;
        self.registration.as_ref().unwrap().end();
        Ok(())
    }
}
impl Drop for Sensor {
    fn drop(&mut self) {
        if self.retain || self.stop().is_err() {
            // An unquiesced foreign callback must never see freed userdata or an
            // unloaded SDK. Retain the bounded failed session until process exit.
            std::mem::forget(self.registration.take());
            std::mem::forget(self.handle.take());
            std::mem::forget(self.state.take());
            std::mem::forget(self.session.clone());
        }
    }
}
pub struct ImuCapture {
    sensors: Vec<Sensor>,
    queue: Arc<Mutex<Queue>>,
    config: ImuConfig,
}
impl Pipeline {
    pub fn start_imu(&self, config: ImuConfig, global: bool) -> Result<ImuCapture> {
        config.validate().map_err(|e| Error(e.to_string()))?;
        if !config.enabled || !self.session.active.get() {
            return Err(Error(
                "IMU requires enabled config and active pipeline".into(),
            ));
        }
        let a = self.session.api();
        let api = a.imu_api().map_err(Error)?;
        let global = if global {
            Some(a.global_timestamp_api().map_err(Error)?)
        } else {
            None
        };
        let queue = Arc::new(Mutex::new(Queue {
            samples: VecDeque::with_capacity(config.queue_capacity),
            error: None,
            progress: ImuProgress::default(),
            last: [Instant::now(); 2],
            received: [0; 2],
            high_water: 0,
        }));
        let mut result = ImuCapture {
            sensors: Vec::with_capacity(2),
            queue: queue.clone(),
            config: config.clone(),
        };
        for (kind, range, rate) in [
            (ImuKind::Accel, config.accel_range_g, config.accel_rate_hz),
            (ImuKind::Gyro, config.gyro_range_dps, config.gyro_rate_hz),
        ] {
            let slot = kind.slot();
            let sensor = Handle::new(a, api.delete_sensor, |e| unsafe {
                (api.sensor)(self.session.device.raw(), 4 + slot as i32, e)
            })?;
            let profiles = Handle::new(a, api.delete_profiles, |e| unsafe {
                (api.profiles)(sensor.raw(), e)
            })?;
            let rates = [25, 50, 100, 200, 500, 1000, 400, 800];
            let rate_codes = [5, 6, 7, 8, 9, 10, 16, 17];
            let rate = rate_codes[rates.iter().position(|v| *v == rate).unwrap()];
            let ranges: &[u32] = if slot == 0 {
                &[2, 4, 8, 16, 3, 6, 12, 24]
            } else {
                &[16, 31, 62, 125, 250, 500, 1000, 2000, 400, 800]
            };
            let range = ranges.iter().position(|v| *v == range).unwrap() as i32 + 1;
            let profile = Handle::new(a, api.delete_profile, |e| unsafe {
                (if slot == 0 {
                    api.accel_profile
                } else {
                    api.gyro_profile
                })(profiles.raw(), range, rate, e)
            })?;
            let mut state = Box::new(CallbackState {
                queue: queue.clone(),
                config: config.clone(),
                kind,
                imu: api,
                index: a.ob_frame_get_index,
                device: a.ob_frame_get_timestamp_us,
                system: a.ob_frame_get_system_timestamp_us,
                global,
                delete: a.ob_delete_frame,
                delete_error: a.ob_delete_error,
            });
            let user = (&mut *state as *mut CallbackState).cast();
            let sensor = Sensor {
                handle: Some(sensor),
                state: Some(state),
                session: self.session.clone(),
                active: true,
                retain: false,
                registration: Some(Registration(Cell::new(false))),
                api,
            };
            sensor.registration.as_ref().unwrap().begin();
            result.sensors.push(sensor);
            let sensor = result.sensors.last().unwrap();
            let mut e = ptr::null_mut();
            unsafe {
                (api.start)(
                    sensor.handle.as_ref().unwrap().raw(),
                    profile.raw(),
                    callback,
                    user,
                    &mut e,
                )
            };
            check(a, e)?;
        }
        Ok(result)
    }
}
impl ImuCapture {
    pub fn drain(&mut self) -> Result<Vec<ImuSample>> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(error) = queue.error {
            return Err(Error(error.into()));
        }
        if queue
            .last
            .iter()
            .any(|at| at.elapsed().as_millis() > self.config.stale_timeout_ms as u128)
        {
            return Err(Error("IMU_STREAM_STALE".into()));
        }
        Ok(queue.samples.drain(..).collect())
    }
    pub fn counters(&self) -> ([u64; 2], usize) {
        let q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        (q.received, q.high_water)
    }
    pub fn stop(&mut self) -> Result<()> {
        if self.sensors.iter().any(|s| s.retain) {
            return Err(Error(
                "IMU stop previously failed; callback state retained".into(),
            ));
        }
        let mut first = None;
        for sensor in &mut self.sensors {
            if let Err(e) = sensor.stop() {
                first.get_or_insert(e);
            }
        }
        if let Some(e) = first {
            // SDK dispatch snapshots both callbacks. A stopped peer's userdata
            // must survive until the entire IMU streamer has quiesced.
            for sensor in &mut self.sensors {
                sensor.retain = true;
            }
            Err(e)
        } else {
            Ok(())
        }
    }
}

impl Drop for ImuCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
