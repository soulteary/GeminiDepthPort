use d2c_core::*;

fn intrinsics(width: u32, height: u32) -> CameraIntrinsics {
    CameraIntrinsics {
        width,
        height,
        fx: 200.,
        fy: 220.,
        cx: 1.,
        cy: 1.,
        skew: 3.,
    }
}
fn camera(model: DistortionModel) -> CameraModel {
    CameraModel::new(intrinsics(4, 3), model).unwrap()
}
fn close(a: f64, b: f64, tolerance: f64) {
    assert!((a - b).abs() <= tolerance, "{a} != {b}");
}

#[test]
fn pinhole_inverts_skew_and_uses_integer_pixel_centers() {
    let model = camera(DistortionModel::PinholeZero);
    let normalized = [0.2, -0.1];
    let pixel = model.project_normalized(normalized).unwrap();
    close(pixel[0], 40.7, 1e-12);
    close(pixel[1], -21., 1e-12);
    let ray = model
        .unproject_pixel(pixel, InverseOptions::default())
        .unwrap();
    close(ray[0], normalized[0], 1e-14);
    close(ray[1], normalized[1], 1e-14);
    assert_eq!(
        model.distort_normalized([1e150, -1e150]).unwrap(),
        [1e150, -1e150]
    );
}

#[test]
fn brown_forward_matches_explicit_radial_and_tangential_equations() {
    let model = camera(DistortionModel::BrownConrady5([
        0.1, 0.02, 0.003, -0.004, 0.005,
    ]));
    let (x, y) = (0.2_f64, -0.3_f64);
    let r2 = x * x + y * y;
    let radial = 1. + 0.1 * r2 + 0.02 * r2 * r2 + 0.005 * r2 * r2 * r2;
    let actual = model.distort_normalized([x, y]).unwrap();
    close(
        actual[0],
        x * radial + 2. * 0.003 * x * y - 0.004 * (r2 + 2. * x * x),
        1e-15,
    );
    close(
        actual[1],
        y * radial + 0.003 * (r2 + 2. * y * y) - 2. * 0.004 * x * y,
        1e-15,
    );
}

#[test]
fn rational_denominator_and_damped_inverse_round_trip() {
    for distortion in [
        DistortionModel::BrownConrady5([0.15, -0.03, 0.005, -0.008, 0.004]),
        DistortionModel::RationalPolynomial8([
            0.15, -0.03, 0.005, -0.008, 0.004, 0.02, -0.003, 0.0004,
        ]),
    ] {
        let model = camera(distortion);
        for point in [[0., 0.], [0.7, -0.5], [-0.45, 0.6], [0.03, -0.05]] {
            let pixel = model.project_normalized(point).unwrap();
            let actual = model
                .unproject_pixel(pixel, InverseOptions::default())
                .unwrap();
            close(actual[0], point[0], 2e-12);
            close(actual[1], point[1], 2e-12);
        }
    }
    let model = camera(DistortionModel::RationalPolynomial8([0.; 8]));
    assert_eq!(model.distort_normalized([1., 0.]).unwrap(), [1., 0.]);
    let singular = camera(DistortionModel::RationalPolynomial8([
        0., 0., 0., 0., 0., -1., 0., 0.,
    ]));
    assert_eq!(
        singular.distort_normalized([1., 0.]),
        Err(ProjectionError::SingularDenominator)
    );
}

#[test]
fn rational_forward_coefficient_order_and_singular_rays_are_explicit() {
    let model = camera(DistortionModel::RationalPolynomial8([
        0.1, 0.02, 0.003, -0.004, 0.005, 0.06, -0.007, 0.0008,
    ]));
    let (x, y) = (0.2_f64, -0.3_f64);
    let r2 = x * x + y * y;
    let radial = (1. + 0.1 * r2 + 0.02 * r2 * r2 + 0.005 * r2 * r2 * r2)
        / (1. + 0.06 * r2 - 0.007 * r2 * r2 + 0.0008 * r2 * r2 * r2);
    let actual = model.distort_normalized([x, y]).unwrap();
    close(
        actual[0],
        x * radial + 2. * 0.003 * x * y - 0.004 * (r2 + 2. * x * x),
        1e-15,
    );
    close(
        actual[1],
        y * radial + 0.003 * (r2 + 2. * y * y) - 2. * 0.004 * x * y,
        1e-15,
    );
    let k = CameraIntrinsics {
        width: 2,
        height: 1,
        fx: 1.,
        fy: 1.,
        cx: 0.,
        cy: 0.,
        skew: 0.,
    };
    let singular_j = CameraModel::new(
        k,
        DistortionModel::BrownConrady5([-1. / 3., 0., 0., 0., 0.]),
    )
    .unwrap();
    assert_eq!(
        singular_j.unproject_pixel([1., 0.], InverseOptions::default()),
        Err(RayFailure::SingularJacobian)
    );
    let singular_d = CameraModel::new(
        k,
        DistortionModel::RationalPolynomial8([0., 0., 0., 0., 0., -1., 0., 0.]),
    )
    .unwrap();
    let table = RayTable::build(
        &singular_d,
        CalibrationId([0; 32]),
        InverseOptions::default(),
        RayTableLimits::default(),
    )
    .unwrap();
    assert_eq!(
        table.rays()[1],
        Ray::Invalid(RayFailure::SingularDenominator)
    );
    assert_eq!(table.stats().singular_denominator, 1);
}

#[test]
fn invalid_configuration_and_v1_unsupported_model_are_typed() {
    let mut k = intrinsics(0, 1);
    assert_eq!(
        CameraModel::new(k, DistortionModel::PinholeZero),
        Err(GeometryError::InvalidDimensions)
    );
    k = intrinsics(16384, 16384);
    assert_eq!(
        CameraModel::new(k, DistortionModel::PinholeZero),
        Err(GeometryError::PixelLimit)
    );
    k = intrinsics(1, 1);
    k.fx = 0.;
    assert_eq!(
        CameraModel::new(k, DistortionModel::PinholeZero),
        Err(GeometryError::NonPositiveFocalLength)
    );
    k = intrinsics(1, 1);
    k.skew = f64::NAN;
    assert_eq!(
        CameraModel::new(k, DistortionModel::PinholeZero),
        Err(GeometryError::NonFiniteIntrinsics)
    );
    assert_eq!(
        CameraModel::new(
            intrinsics(1, 1),
            DistortionModel::BrownConrady5([f64::NAN; 5])
        ),
        Err(GeometryError::NonFiniteDistortion)
    );
    assert_eq!(
        CameraModel::new(intrinsics(1, 1), DistortionModel::Equidistant4([0.; 4])),
        Err(GeometryError::UnsupportedDistortionModel)
    );
    assert_eq!(
        InverseOptions::new(0, 1e-8),
        Err(GeometryError::InvalidInverseOptions)
    );
    assert_eq!(
        InverseOptions::new(129, 1e-8),
        Err(GeometryError::InvalidInverseOptions)
    );
    assert_eq!(
        InverseOptions::new(4, f64::NAN),
        Err(GeometryError::InvalidInverseOptions)
    );
}

#[test]
fn failed_inverse_is_marked_per_ray_and_bounded_not_silently_pinhole() {
    let k = CameraIntrinsics {
        width: 3,
        height: 1,
        fx: 1.,
        fy: 1.,
        cx: 0.,
        cy: 0.,
        skew: 0.,
    };
    let model = CameraModel::new(k, DistortionModel::BrownConrady5([1., 0., 0., 0., 0.])).unwrap();
    let options = InverseOptions::new(1, 1e-15).unwrap();
    let table = RayTable::build(
        &model,
        CalibrationId([1; 32]),
        options,
        RayTableLimits::default(),
    )
    .unwrap();
    assert_eq!(table.rays()[0], Ray::Valid { x: 0., y: 0. });
    assert!(matches!(
        table.rays()[1],
        Ray::Invalid(RayFailure::NonConvergent)
    ));
    assert_eq!(table.stats().valid + table.stats().invalid, 3);
    assert!(table.stats().nonconvergent >= 1);
    assert_eq!(
        model.unproject_pixel([f64::INFINITY, 0.], options),
        Err(RayFailure::NonFinite)
    );
}

#[test]
fn ray_key_binds_full_calibration_model_geometry_and_solver_version() {
    let options = InverseOptions::default();
    let model = camera(DistortionModel::PinholeZero);
    let id = CalibrationId([2; 32]);
    let key = RayTableKey::new(&model, id, options);
    assert_eq!(key.algorithm_version(), RAY_ALGORITHM_VERSION);
    assert_ne!(
        key,
        RayTableKey::new(&model, CalibrationId([3; 32]), options)
    );
    assert_ne!(
        key,
        RayTableKey::new(&model, id, InverseOptions::new(3, 1e-8).unwrap())
    );
    let mut k = model.intrinsics();
    k.skew += 1.;
    let other = CameraModel::new(k, DistortionModel::PinholeZero).unwrap();
    assert_ne!(key, RayTableKey::new(&other, id, options));
    k = model.intrinsics();
    k.width += 1;
    let resized = CameraModel::new(k, DistortionModel::PinholeZero).unwrap();
    assert_ne!(key, RayTableKey::new(&resized, id, options));
    let brown = camera(DistortionModel::BrownConrady5([0.; 5]));
    assert_ne!(key, RayTableKey::new(&brown, id, options));
}

#[test]
fn cache_lru_reuses_storage_and_obeys_entry_and_payload_bounds() {
    let model = camera(DistortionModel::PinholeZero);
    let options = InverseOptions::default();
    let one = RayTable::build(
        &model,
        CalibrationId([0; 32]),
        options,
        RayTableLimits::default(),
    )
    .unwrap()
    .allocated_payload_bytes();
    let mut cache = RayTableCache::new(RayTableCacheLimits {
        max_entries: 2,
        max_bytes: one * 2,
        max_table_pixels: 12,
    })
    .unwrap();
    let pointer = cache
        .get_or_build(&model, CalibrationId([1; 32]), options)
        .unwrap()
        .rays()
        .as_ptr();
    assert_eq!(
        pointer,
        cache
            .get_or_build(&model, CalibrationId([1; 32]), options)
            .unwrap()
            .rays()
            .as_ptr()
    );
    assert_eq!((cache.hits(), cache.builds()), (1, 1));
    cache
        .get_or_build(&model, CalibrationId([2; 32]), options)
        .unwrap();
    cache
        .get_or_build(&model, CalibrationId([1; 32]), options)
        .unwrap();
    cache
        .get_or_build(&model, CalibrationId([3; 32]), options)
        .unwrap();
    assert_eq!(cache.len(), 2);
    assert!(cache.allocated_payload_bytes() <= one * 2);
    let before = cache.builds();
    cache
        .get_or_build(&model, CalibrationId([2; 32]), options)
        .unwrap();
    assert_eq!(cache.builds(), before + 1);
    let result = RayTable::build(
        &model,
        CalibrationId([0; 32]),
        options,
        RayTableLimits {
            max_pixels: 12,
            max_bytes: one - 1,
        },
    );
    assert!(matches!(result, Err(GeometryError::MemoryLimit)));
    assert!(matches!(
        RayTableCache::new(RayTableCacheLimits {
            max_entries: 0,
            max_bytes: 1000,
            max_table_pixels: 12
        }),
        Err(GeometryError::InvalidMemoryLimits)
    ));
}

#[test]
fn ray_cache_refuses_oversized_tables_without_evicting_a_valid_entry() {
    let model = camera(DistortionModel::PinholeZero);
    let options = InverseOptions::default();
    let id = CalibrationId([7; 32]);
    let mut cache = RayTableCache::new(RayTableCacheLimits {
        max_entries: 2,
        max_bytes: 12 * std::mem::size_of::<Ray>(),
        max_table_pixels: 12,
    })
    .unwrap();
    assert!(cache.is_empty());
    let table = cache.get_or_build(&model, id, options).unwrap();
    assert_eq!(table.key().calibration_id(), id);
    assert_eq!(table.dimensions(), (4, 3));
    assert_eq!(table.stats().valid, 12);
    let larger = CameraModel::new(intrinsics(5, 3), DistortionModel::PinholeZero).unwrap();
    assert!(matches!(
        cache.get_or_build(&larger, id, options),
        Err(GeometryError::PixelLimit)
    ));
    assert_eq!((cache.len(), cache.builds()), (1, 1));
    cache.get_or_build(&model, id, options).unwrap();
    assert_eq!(cache.hits(), 1);
    let mut memory_limited = RayTableCache::new(RayTableCacheLimits {
        max_entries: 1,
        max_bytes: std::mem::size_of::<Ray>(),
        max_table_pixels: 12,
    })
    .unwrap();
    assert!(matches!(
        memory_limited.get_or_build(&model, id, options),
        Err(GeometryError::MemoryLimit)
    ));
    assert!(memory_limited.is_empty());
    for limits in [
        RayTableLimits {
            max_pixels: 0,
            max_bytes: 1000,
        },
        RayTableLimits {
            max_pixels: 12,
            max_bytes: 0,
        },
    ] {
        assert!(matches!(
            RayTable::build(&model, id, options, limits),
            Err(GeometryError::InvalidMemoryLimits)
        ));
    }
    assert!(matches!(
        RayTable::build(
            &model,
            id,
            options,
            RayTableLimits {
                max_pixels: 11,
                max_bytes: 1000
            }
        ),
        Err(GeometryError::PixelLimit)
    ));
}

#[test]
fn extreme_finite_coordinates_fail_closed_instead_of_emitting_nonfinite_rays() {
    let pinhole = camera(DistortionModel::PinholeZero);
    assert_eq!(pinhole.distortion(), DistortionModel::PinholeZero);
    assert_eq!(
        pinhole.project_normalized([f64::MAX, 0.]),
        Err(ProjectionError::NonFinite)
    );
    let brown = camera(DistortionModel::BrownConrady5([1., 0., 0., 0., 0.]));
    assert_eq!(
        brown.distort_normalized([1e100, 0.]),
        Err(ProjectionError::NonFinite)
    );
    assert_eq!(
        brown.unproject_pixel([1e100, 0.], InverseOptions::default()),
        Err(RayFailure::NonFinite)
    );
    // A tiny focal length makes finite sensor coordinates unrepresentable in normalized space.
    let mut k = intrinsics(1, 1);
    k.fx = f64::MIN_POSITIVE;
    k.cx = f64::MAX;
    let model = CameraModel::new(k, DistortionModel::PinholeZero).unwrap();
    let table = RayTable::build(
        &model,
        CalibrationId([0; 32]),
        InverseOptions::default(),
        RayTableLimits::default(),
    )
    .unwrap();
    assert_eq!(table.stats().nonfinite, 1);
    assert_eq!(table.rays(), &[Ray::Invalid(RayFailure::NonFinite)]);
}
