//! Actual enabled-profile calibration read through the existing pipeline session.
use super::*;

#[derive(Debug, Clone)]
pub struct StreamCalibration {
    pub profile: VideoProfile,
    pub format: Format,
    pub fx: f64,
    pub fy: f64,
    pub cx: f64,
    pub cy: f64,
    pub distortion_model: String,
    pub distortion: Vec<f64>,
}
#[derive(Debug, Clone)]
pub struct Extrinsics {
    /// SDK row-major R in p_color_m = R * p_depth_m + translation_m.
    pub rotation_row_major: [f64; 9],
    pub translation_m: [f64; 3],
}
impl Extrinsics {
    pub fn from_sdk(rotation_row_major: [f32; 9], translation_mm: [f32; 3]) -> Result<Self> {
        let value = Self {
            rotation_row_major: rotation_row_major.map(f64::from),
            translation_m: translation_mm.map(|v| f64::from(v) / 1000.),
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if !self
            .rotation_row_major
            .iter()
            .chain(self.translation_m.iter())
            .all(|x| x.is_finite())
        {
            return Err(Error("nonfinite SDK extrinsics".into()));
        }
        let r = &self.rotation_row_major;
        for row in 0..3 {
            for col in 0..3 {
                let dot: f64 = (0..3).map(|j| r[row * 3 + j] * r[col * 3 + j]).sum();
                if (dot - if row == col { 1. } else { 0. }).abs() > 1e-5 {
                    return Err(Error("SDK extrinsic rotation is not orthonormal".into()));
                }
            }
        }
        let det = r[0] * (r[4] * r[8] - r[5] * r[7]) - r[1] * (r[3] * r[8] - r[5] * r[6])
            + r[2] * (r[3] * r[7] - r[4] * r[6]);
        if (det - 1.).abs() > 1e-5 {
            return Err(Error("SDK extrinsic rotation is not proper".into()));
        }
        Ok(())
    }
    pub fn transform_depth_to_color(&self, depth_m: [f64; 3]) -> [f64; 3] {
        std::array::from_fn(|row| {
            (0..3)
                .map(|col| self.rotation_row_major[row * 3 + col] * depth_m[col])
                .sum::<f64>()
                + self.translation_m[row]
        })
    }
}
#[derive(Debug, Clone)]
pub struct SessionCalibration {
    pub color: Option<StreamCalibration>,
    pub depth: Option<StreamCalibration>,
    pub depth_to_color: Option<Extrinsics>,
}

struct ProfileOwner {
    ptr: NonNull<sys::Handle>,
    api: Rc<sys::Api>,
    delete: unsafe extern "C" fn(*const sys::Handle, sys::ErrorOut),
}
impl ProfileOwner {
    fn new(
        api: &Rc<sys::Api>,
        delete: unsafe extern "C" fn(*const sys::Handle, sys::ErrorOut),
        f: impl FnOnce(sys::ErrorOut) -> *mut sys::Handle,
    ) -> Result<Self> {
        let mut e = ptr::null_mut();
        let raw = f(&mut e);
        let owned = NonNull::new(raw).map(|ptr| Self {
            ptr,
            api: api.clone(),
            delete,
        });
        check(api, e)?;
        owned.ok_or_else(|| Error("SDK returned null profile object".into()))
    }
    fn raw(&self) -> *mut sys::Handle {
        self.ptr.as_ptr()
    }
    fn read<T>(
        &self,
        f: unsafe extern "C" fn(*const sys::Handle, sys::ErrorOut) -> T,
    ) -> Result<T> {
        let mut e = ptr::null_mut();
        let value = unsafe { f(self.raw(), &mut e) };
        check(&self.api, e)?;
        Ok(value)
    }
}
impl Drop for ProfileOwner {
    fn drop(&mut self) {
        let mut e = ptr::null_mut();
        unsafe { (self.delete)(self.raw(), &mut e) };
        let _ = check(&self.api, e);
    }
}

fn distortion(d: sys::CameraDistortion) -> Result<(String, Vec<f64>)> {
    let values = [d.k1, d.k2, d.k3, d.k4, d.k5, d.k6, d.p1, d.p2];
    if !values.iter().all(|x| x.is_finite()) {
        return Err(Error("nonfinite SDK distortion".into()));
    }
    // Rust 1.85 match guards can borrow the scrutinee; never borrow a packed field.
    let sdk_model = d.model;
    let (model, coefficients) = match sdk_model {
        0 if values.iter().all(|x| *x == 0.) => ("plumb_bob", vec![0.; 5]),
        3 if d.k4 == 0. && d.k5 == 0. && d.k6 == 0. => {
            ("plumb_bob", vec![d.k1, d.k2, d.p1, d.p2, d.k3])
        }
        4 => (
            "rational_polynomial",
            vec![d.k1, d.k2, d.p1, d.p2, d.k3, d.k4, d.k5, d.k6],
        ),
        5 if d.k5 == 0. && d.k6 == 0. && d.p1 == 0. && d.p2 == 0. => {
            ("equidistant", vec![d.k1, d.k2, d.k3, d.k4])
        }
        _ => {
            return Err(Error(
                "SDK distortion model cannot be represented exactly by supported ROS model".into(),
            ));
        }
    };
    Ok((
        model.into(),
        coefficients.into_iter().map(f64::from).collect(),
    ))
}

impl Pipeline {
    /// Read calibration from actual enabled profiles on this already-open
    /// pipeline. No second Context/device, default profile or property writes.
    pub fn calibration(&self) -> Result<SessionCalibration> {
        if !self.session.active.get() {
            return Err(Error("calibration requires active pipeline".into()));
        }
        let api = self.session.api();
        let a = api.calibration_api().map_err(Error)?;
        let config = Handle::new(api, api.ob_delete_config, |e| unsafe {
            (a.pipeline_config)(self.session.pipeline.raw(), e)
        })?;
        let list = ProfileOwner::new(api, a.delete_profiles, |e| unsafe {
            (a.config_profiles)(config.raw(), e)
        })?;
        let count = list.read(a.profile_count)?;
        if count == 0 || count > 2 {
            return Err(Error("unexpected active profile count".into()));
        }
        let mut profiles = Vec::new();
        for index in 0..count {
            profiles.push(ProfileOwner::new(api, a.delete_profile, |e| unsafe {
                (a.profile_get)(list.raw(), index as i32, e)
            })?);
        }
        read_calibration(&self.session, profiles, a)
    }
}

impl FrameSet {
    /// Calibrate from profiles attached to actual received frames under this
    /// frameset/session. Both requested streams must be present in this set.
    pub fn calibration(&self) -> Result<SessionCalibration> {
        let session = &self.frame.session;
        let api = session.api();
        let a = api.calibration_api().map_err(Error)?;
        let mut profiles = Vec::new();
        for frame in [self.color()?, self.depth()?].into_iter().flatten() {
            profiles.push(ProfileOwner::new(api, a.delete_profile, |e| unsafe {
                (a.frame_profile)(frame.ptr.as_ptr(), e)
            })?);
        }
        read_calibration(session, profiles, a)
    }
}
fn read_calibration(
    session: &Session,
    profiles: Vec<ProfileOwner>,
    a: sys::CalibrationApi,
) -> Result<SessionCalibration> {
    let api = session.api();
    let mut color = None;
    let mut depth = None;
    let mut color_profile = None;
    let mut depth_profile = None;
    for profile in profiles {
        let kind = profile.read(a.profile_type)?;
        let (requested, expected_format) = match kind {
            sys::STREAM_COLOR => (session.color_requested, sys::FORMAT_YUYV),
            sys::STREAM_DEPTH => (session.depth_requested, sys::FORMAT_Y16),
            _ => return Err(Error("unexpected active stream type".into())),
        };
        let actual = VideoProfile {
            width: profile.read(a.width)?,
            height: profile.read(a.height)?,
            fps: profile.read(a.fps)?,
        };
        let format = profile.read(a.profile_format)?;
        if requested != Some(actual) || format != expected_format {
            return Err(Error(
                "active SDK profile differs from exact requested profile".into(),
            ));
        }
        let intrinsic = profile.read(a.intrinsic)?;
        if intrinsic.width <= 0
            || intrinsic.height <= 0
            || intrinsic.width as u32 != actual.width
            || intrinsic.height as u32 != actual.height
            || ![intrinsic.fx, intrinsic.fy, intrinsic.cx, intrinsic.cy]
                .iter()
                .all(|x| x.is_finite())
            || intrinsic.fx <= 0.
            || intrinsic.fy <= 0.
        {
            return Err(Error("SDK intrinsics do not match active profile".into()));
        }
        let (distortion_model, distortion) = distortion(profile.read(a.distortion)?)?;
        let value = StreamCalibration {
            profile: actual,
            format: if kind == sys::STREAM_COLOR {
                Format::Yuyv
            } else {
                Format::Depth16
            },
            fx: intrinsic.fx.into(),
            fy: intrinsic.fy.into(),
            cx: intrinsic.cx.into(),
            cy: intrinsic.cy.into(),
            distortion_model,
            distortion,
        };
        if kind == sys::STREAM_COLOR {
            if color.is_some() {
                return Err(Error("duplicate color profile".into()));
            }
            color = Some(value);
            color_profile = Some(profile)
        } else {
            if depth.is_some() {
                return Err(Error("duplicate depth profile".into()));
            }
            depth = Some(value);
            depth_profile = Some(profile)
        }
    }
    if color.is_some() != session.color_requested.is_some()
        || depth.is_some() != session.depth_requested.is_some()
    {
        return Err(Error("active SDK profile missing".into()));
    }
    let depth_to_color = if let (Some(d), Some(c)) = (&depth_profile, &color_profile) {
        let mut e = ptr::null_mut();
        let raw = unsafe { (a.extrinsic_to)(d.raw(), c.raw(), &mut e) };
        check(api, e)?;
        Some(Extrinsics::from_sdk(raw.rot, raw.trans)?)
    } else {
        None
    };
    Ok(SessionCalibration {
        color,
        depth,
        depth_to_color,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn depth_to_color_direction_units_and_rotation_are_explicit() {
        let e = Extrinsics::from_sdk([0., -1., 0., 1., 0., 0., 0., 0., 1.], [100., 200., 300.])
            .unwrap();
        let p = e.transform_depth_to_color([1., 2., 3.]);
        assert_eq!(p, [-1.9, 1.2, 3.3]);
        assert!(Extrinsics::from_sdk([-1., 0., 0., 0., 1., 0., 0., 0., 1.], [0.; 3]).is_err());
        assert!(Extrinsics::from_sdk([1.; 9], [0.; 3]).is_err());
        assert!(
            Extrinsics::from_sdk([1., 0., 0., 0., 1., 0., 0., 0., 1.], [f32::NAN, 0., 0.]).is_err()
        );
    }
    #[test]
    fn distortion_mapping_preserves_model_and_order() {
        let raw = sys::CameraDistortion {
            k1: 1.,
            k2: 2.,
            k3: 3.,
            k4: 4.,
            k5: 5.,
            k6: 6.,
            p1: 7.,
            p2: 8.,
            model: 4,
        };
        let (m, d) = distortion(raw).unwrap();
        assert_eq!(m, "rational_polynomial");
        assert_eq!(d, [1., 2., 7., 8., 3., 4., 5., 6.]);
        assert!(distortion(sys::CameraDistortion { model: 1, ..raw }).is_err());
        assert!(distortion(sys::CameraDistortion { model: 2, ..raw }).is_err());
        assert!(distortion(sys::CameraDistortion { model: 3, ..raw }).is_err());
    }
    #[test]
    fn finite_zero_and_equidistant_distortion_models_keep_their_exact_parameter_order() {
        let zero = sys::CameraDistortion {
            k1: 0.,
            k2: 0.,
            k3: 0.,
            k4: 0.,
            k5: 0.,
            k6: 0.,
            p1: 0.,
            p2: 0.,
            model: 0,
        };
        assert_eq!(distortion(zero).unwrap(), ("plumb_bob".into(), vec![0.; 5]));
        let equidistant = sys::CameraDistortion {
            k1: 1.,
            k2: 2.,
            k3: 3.,
            k4: 4.,
            model: 5,
            ..zero
        };
        assert_eq!(
            distortion(equidistant).unwrap(),
            ("equidistant".into(), vec![1., 2., 3., 4.])
        );
        assert!(
            distortion(sys::CameraDistortion {
                k1: f32::NAN,
                ..zero
            })
            .unwrap_err()
            .to_string()
            .contains("nonfinite")
        );
    }
}
