use d2c_core::*;
const I: [f64; 9] = [1., 0., 0., 0., 1., 0., 0., 0., 1.];
fn camera(w: u32, h: u32) -> CameraModel {
    CameraModel::new(
        CameraIntrinsics {
            width: w,
            height: h,
            fx: 1.,
            fy: 1.,
            cx: 0.,
            cy: 0.,
            skew: 0.,
        },
        DistortionModel::PinholeZero,
    )
    .unwrap()
}
fn rays(m: &CameraModel) -> RayTable {
    RayTable::build(
        m,
        CalibrationId([0; 32]),
        InverseOptions::default(),
        RayTableLimits::default(),
    )
    .unwrap()
}
fn buffer() -> WarpOutputBuffer {
    WarpOutputBuffer::new(WarpBufferLimits::default()).unwrap()
}
fn bytes(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
fn raw(b: &[u8], w: u32, h: u32, scale: f64) -> RawDepthView<'_> {
    RawDepthView::new(
        b,
        w,
        h,
        w as usize * 2,
        Endian::Little,
        DepthScale::millimeters_per_count(scale).unwrap(),
    )
    .unwrap()
}
#[test]
fn identity_zero_max_and_buffer_reuse() {
    let m = camera(3, 1);
    let lut = rays(&m);
    let b = bytes(&[0, 1, 65535]);
    let mut out = buffer();
    let a = warp_depth_to_color(
        raw(&b, 3, 1, 1.),
        &lut,
        &m,
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.depth_mm, [0, 1, 65535]);
    assert_eq!(a.validity, [0, 1, 1]);
    assert_eq!(a.source_indices, [usize::MAX, 1, 2]);
    let ptr = a.depth_mm.as_ptr();
    let mut packed = vec![0; 6];
    a.write_packed_le(&mut packed).unwrap();
    assert_eq!(packed, b);
    let a = warp_depth_to_color(
        raw(&b, 3, 1, 1.),
        &lut,
        &m,
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.depth_mm.as_ptr(), ptr);
}
#[test]
fn translation_uses_color_z_not_source_z_and_different_dimensions() {
    let m = camera(1, 1);
    let c = camera(2, 2);
    let lut = rays(&m);
    let b = bytes(&[1000]);
    let mut out = buffer();
    let a = warp_depth_to_color(
        raw(&b, 1, 1, 1.),
        &lut,
        &c,
        DepthToColorTransform::new(I, [1., 0., 1.]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.depth_mm, [0, 2000, 0, 0]);
}
#[test]
fn rotation_direction_and_behind_camera() {
    let m = camera(1, 1);
    let lut = rays(&m);
    let b = bytes(&[1000]);
    let mut out = buffer();
    let r = [0., 0., 1., 0., 1., 0., -1., 0., 0.];
    let a = warp_depth_to_color(
        raw(&b, 1, 1, 1.),
        &lut,
        &camera(3, 1),
        DepthToColorTransform::new(r, [0., 0., 1.]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.depth_mm, [0, 1000, 0]);
    let inverse = [0., 0., -1., 0., 1., 0., 1., 0., 0.];
    let a = warp_depth_to_color(
        raw(&b, 1, 1, 1.),
        &lut,
        &camera(3, 1),
        DepthToColorTransform::new(inverse, [0., 0., 1.]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.stats.out_of_bounds, 1);
    let a = warp_depth_to_color(
        raw(&b, 1, 1, 1.),
        &lut,
        &m,
        DepthToColorTransform::new(I, [0., 0., -2.]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.stats.behind, 1);
}
#[test]
fn zbuffer_precedes_quantization_and_exact_ties_choose_row_major() {
    let m = camera(2, 1);
    let lut = rays(&m);
    let c = CameraModel::new(
        CameraIntrinsics {
            width: 1,
            height: 1,
            fx: 0.1,
            fy: 1.,
            cx: 0.,
            cy: 0.,
            skew: 0.,
        },
        DistortionModel::PinholeZero,
    )
    .unwrap();
    let mut out = buffer();
    for (values, winner) in [([10004, 10003], 1), ([10003, 10003], 0)] {
        let b = bytes(&values);
        let a = warp_depth_to_color(
            raw(&b, 2, 1, 0.1),
            &lut,
            &c,
            DepthToColorTransform::new(I, [0.; 3]).unwrap(),
            &mut out,
        )
        .unwrap();
        assert_eq!(a.depth_mm, [1000]);
        assert_eq!(a.source_indices, [winner]);
        assert_eq!(a.stats.collisions, 1);
        if winner == 0 {
            assert_eq!(a.stats.equal_z_ties, 1)
        }
    }
}
#[test]
fn overflow_is_whole_frame_and_small_nonzero_is_clamped() {
    let m = camera(2, 1);
    let lut = rays(&m);
    let mut out = buffer();
    let b = bytes(&[1, 65535]);
    let e = warp_depth_to_color(
        raw(&b, 2, 1, 2.),
        &lut,
        &m,
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut out,
    )
    .unwrap_err();
    assert_eq!(e, WarpError::OutputOverflow);
    assert!(out.output().is_none());
    let b = bytes(&[1, 2]);
    let a = warp_depth_to_color(
        raw(&b, 2, 1, 0.1),
        &lut,
        &m,
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.depth_mm, [1, 1]);
    assert_eq!(a.stats.clamped_nonzero, 2);
}
#[test]
fn endian_odd_padding_and_truncation() {
    let m = camera(1, 2);
    let lut = rays(&m);
    let b = [0x03, 0xe8, 99, 0x07, 0xd0, 88];
    let raw = RawDepthView::new(
        &b,
        1,
        2,
        3,
        Endian::Big,
        DepthScale::millimeters_per_count(1.).unwrap(),
    )
    .unwrap();
    let mut out = buffer();
    let a = warp_depth_to_color(
        raw,
        &lut,
        &m,
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.depth_mm, [1000, 2000]);
    assert!(matches!(
        RawDepthView::new(
            &b[..5],
            1,
            2,
            3,
            Endian::Big,
            DepthScale::millimeters_per_count(1.).unwrap()
        ),
        Err(WarpError::PayloadLength)
    ));
    assert!(matches!(
        RawDepthView::new(
            &b,
            1,
            2,
            1,
            Endian::Big,
            DepthScale::millimeters_per_count(1.).unwrap()
        ),
        Err(WarpError::InvalidStride)
    ));
}
#[test]
fn signed_nearest_edge_bounds() {
    let m = camera(1, 1);
    let lut = rays(&m);
    let b = bytes(&[1000]);
    let mut out = buffer();
    for (x, valid) in [
        (-0.5, true),
        (-0.5000001, false),
        (0.4999999, true),
        (0.5, false),
    ] {
        let a = warp_depth_to_color(
            raw(&b, 1, 1, 1.),
            &lut,
            &m,
            DepthToColorTransform::new(I, [x, 0., 0.]).unwrap(),
            &mut out,
        )
        .unwrap();
        assert_eq!(a.stats.valid_output, usize::from(valid));
    }
}
#[test]
fn invalid_transforms_scales_and_limits() {
    assert!(DepthToColorTransform::new([-1., 0., 0., 0., 1., 0., 0., 0., 1.], [0.; 3]).is_err());
    assert!(DepthToColorTransform::new([2.; 9], [0.; 3]).is_err());
    assert!(DepthToColorTransform::new(I, [f64::NAN, 0., 0.]).is_err());
    for scale in [0., -1., f64::NAN, f64::INFINITY, f64::MAX] {
        assert!(DepthScale::millimeters_per_count(scale).is_err())
    }
    let m = camera(1, 1);
    let lut = rays(&m);
    let b = bytes(&[1]);
    let mut out = WarpOutputBuffer::new(WarpBufferLimits {
        max_pixels: 1,
        max_bytes: 1,
    })
    .unwrap();
    assert_eq!(
        warp_depth_to_color(
            raw(&b, 1, 1, 1.),
            &lut,
            &m,
            DepthToColorTransform::new(I, [0.; 3]).unwrap(),
            &mut out
        )
        .unwrap_err(),
        WarpError::MemoryLimit
    );
    assert!(out.output().is_none());
}
#[test]
fn distortion_is_applied_in_both_directions() {
    let m = CameraModel::new(
        CameraIntrinsics {
            width: 3,
            height: 1,
            fx: 2.,
            fy: 2.,
            cx: 1.,
            cy: 0.,
            skew: 0.,
        },
        DistortionModel::BrownConrady5([0.1, 0., 0., 0., 0.]),
    )
    .unwrap();
    let lut = rays(&m);
    let b = bytes(&[1000, 1000, 1000]);
    let mut out = buffer();
    let a = warp_depth_to_color(
        raw(&b, 3, 1, 1.),
        &lut,
        &m,
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.depth_mm, [1000; 3]);
    let pinhole = CameraModel::new(m.intrinsics(), DistortionModel::PinholeZero).unwrap();
    let far = CameraModel::new(
        CameraIntrinsics {
            width: 8,
            height: 1,
            fx: 2.,
            fy: 1.,
            cx: 0.,
            cy: 0.,
            skew: 0.,
        },
        DistortionModel::BrownConrady5([1., 0., 0., 0., 0.]),
    )
    .unwrap();
    let p = rays(&pinhole);
    let b = bytes(&[0, 0, 1000]);
    let a = warp_depth_to_color(
        raw(&b, 3, 1, 1.),
        &p,
        &far,
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.source_indices[1], 2);
}
#[test]
fn inverse_failure_does_not_turn_into_a_valid_depth_and_dimension_errors_clear_output() {
    let m = CameraModel::new(
        CameraIntrinsics {
            width: 2,
            height: 1,
            fx: 1.,
            fy: 1.,
            cx: 0.,
            cy: 0.,
            skew: 0.,
        },
        DistortionModel::BrownConrady5([100., 0., 0., 0., 0.]),
    )
    .unwrap();
    let lut = RayTable::build(
        &m,
        CalibrationId([0; 32]),
        InverseOptions::new(1, 1e-15).unwrap(),
        RayTableLimits::default(),
    )
    .unwrap();
    assert!(lut.stats().invalid > 0);
    let b = bytes(&[1000, 1000]);
    let mut out = buffer();
    let a = warp_depth_to_color(
        raw(&b, 2, 1, 1.),
        &lut,
        &camera(2, 1),
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(a.stats.invalid_ray, lut.stats().invalid);
    let single = bytes(&[1000]);
    assert_eq!(
        warp_depth_to_color(
            raw(&single, 1, 1, 1.),
            &lut,
            &camera(2, 1),
            DepthToColorTransform::new(I, [0.; 3]).unwrap(),
            &mut out
        )
        .unwrap_err(),
        WarpError::RayDimensions
    );
    assert!(out.output().is_none());
}

#[test]
fn extreme_geometry_is_counted_without_nonfinite_output_and_packing_checks_length() {
    let mut k = camera(1, 1).intrinsics();
    k.cx = -1e100;
    let source = CameraModel::new(k, DistortionModel::PinholeZero).unwrap();
    let lut = rays(&source);
    let pixels = bytes(&[65535]);
    let mut out = buffer();
    let scale = DepthScale::millimeters_per_count(1e200).unwrap();
    assert_eq!(scale.value(), 1e200);
    let view = RawDepthView::new(&pixels, 1, 1, 2, Endian::Little, scale).unwrap();
    let result = warp_depth_to_color(
        view,
        &lut,
        &camera(1, 1),
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut out,
    )
    .unwrap();
    // All projection coordinates remain finite here but cannot hit the sensor.
    assert_eq!(result.stats.out_of_bounds, 1);
    assert_eq!(
        result.write_packed_le(&mut [0]),
        Err(WarpError::OutputLength)
    );
    let distorted = CameraModel::new(
        camera(1, 1).intrinsics(),
        DistortionModel::BrownConrady5([1.; 5]),
    )
    .unwrap();
    let result = warp_depth_to_color(
        raw(&pixels, 1, 1, 1.),
        &lut,
        &distorted,
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut out,
    )
    .unwrap();
    assert_eq!(result.stats.projection_failure, 1);
    let err = RawDepthView::new(
        &[],
        0,
        1,
        0,
        Endian::Little,
        DepthScale::millimeters_per_count(1.).unwrap(),
    )
    .unwrap_err();
    assert_eq!(err, WarpError::Geometry(GeometryError::InvalidDimensions));
    assert!(err.to_string().contains("InvalidDimensions"));
    assert!(matches!(
        WarpOutputBuffer::new(WarpBufferLimits {
            max_pixels: 0,
            max_bytes: 10
        }),
        Err(WarpError::MemoryLimit)
    ));
}

#[test]
fn finite_source_rays_that_overflow_transformed_coordinates_are_invalid_output() {
    let mut k = camera(1, 1).intrinsics();
    k.cx = -1e200;
    let source = CameraModel::new(k, DistortionModel::PinholeZero).unwrap();
    let lut = rays(&source);
    let pixels = bytes(&[65535]);
    let mut buffer = buffer();
    let result = warp_depth_to_color(
        raw(&pixels, 1, 1, 1e200),
        &lut,
        &camera(1, 1),
        DepthToColorTransform::new(I, [0.; 3]).unwrap(),
        &mut buffer,
    )
    .unwrap();
    assert_eq!(result.stats.nonfinite, 1);
    assert_eq!(result.depth_mm, [0]);
    assert_eq!(result.validity, [0]);
}
