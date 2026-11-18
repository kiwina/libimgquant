mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{gradient, photo_like};
use libimgquant::{Attributes, Error, RGBA};

#[test]
fn defaults() {
    let attr = Attributes::new();
    assert_eq!(attr.max_colors(), 256);
    assert_eq!(attr.speed(), 3);
    assert_eq!(attr.quality(), (0, 100));
    assert_eq!(attr.min_opacity(), 255);
    assert_eq!(attr.min_posterization(), 0);
}

#[test]
fn setter_validation() {
    let mut attr = libimgquant::new();
    assert_eq!(attr.set_max_colors(1), Err(Error::ValueOutOfRange));
    assert_eq!(attr.set_max_colors(257), Err(Error::ValueOutOfRange));
    assert_eq!(attr.set_max_colors(2), Ok(()));
    assert_eq!(attr.set_speed(0), Err(Error::ValueOutOfRange));
    assert_eq!(attr.set_speed(11), Err(Error::ValueOutOfRange));
    assert_eq!(attr.set_quality(50, 40), Err(Error::ValueOutOfRange));
    assert_eq!(attr.set_quality(50, 101), Err(Error::ValueOutOfRange));
    assert_eq!(attr.set_min_posterization(5), Err(Error::ValueOutOfRange));
    assert_eq!(attr.max_colors(), 2);
    assert_eq!(attr.speed(), 3);
}

#[test]
fn getters_round_trip() {
    let mut attr = libimgquant::new();
    for (min, max) in [(0, 0), (0, 100), (60, 80), (100, 100), (1, 1), (37, 99)] {
        attr.set_quality(min, max).unwrap();
        assert_eq!(attr.quality(), (min, max));
    }
    for v in [0, 1, 100, 200, 254, 255] {
        attr.set_min_opacity(v).unwrap();
        assert_eq!(attr.min_opacity(), v);
    }
    for s in 1..=10 {
        attr.set_speed(s).unwrap();
        assert_eq!(attr.speed(), s as u32);
    }
}

#[test]
fn result_setter_validation() {
    let attr = libimgquant::new();
    let mut image = attr.new_image(gradient(8, 8), 8, 8, 0.0).unwrap();
    let mut res = attr.quantize(&mut image).unwrap();
    assert_eq!(res.dithering_level(), 0.0);
    assert_eq!(res.set_dithering_level(-0.1), Err(Error::ValueOutOfRange));
    assert_eq!(res.set_dithering_level(1.1), Err(Error::ValueOutOfRange));
    assert_eq!(res.set_dithering_level(f32::NAN), Err(Error::ValueOutOfRange));
    res.set_dithering_level(0.5).unwrap();
    assert_eq!(res.dithering_level(), 0.5);
    assert_eq!(res.set_output_gamma(0.0), Err(Error::ValueOutOfRange));
    assert_eq!(res.set_output_gamma(1.0), Err(Error::ValueOutOfRange));
    assert_eq!(res.set_output_gamma(f64::NAN), Err(Error::ValueOutOfRange));
}

#[test]
fn image_validation() {
    let attr = libimgquant::new();
    let px = gradient(4, 4);
    assert_eq!(
        attr.new_image(px.clone(), 0, 4, 0.0).err(),
        Some(Error::ValueOutOfRange)
    );
    assert_eq!(
        attr.new_image(px.clone(), 4, 0, 0.0).err(),
        Some(Error::ValueOutOfRange)
    );
    assert_eq!(attr.new_image(px.clone(), 5, 4, 0.0).err(), Some(Error::BufferTooSmall));
    assert_eq!(
        attr.new_image(px.clone(), 4, 4, -0.1).err(),
        Some(Error::ValueOutOfRange)
    );
    assert_eq!(
        attr.new_image(px.clone(), 4, 4, 1.5).err(),
        Some(Error::ValueOutOfRange)
    );
    assert_eq!(
        attr.new_image(px.clone(), 4, 4, f64::NAN).err(),
        Some(Error::ValueOutOfRange)
    );
    assert_eq!(
        attr.new_image_stride(px.clone(), 4, 4, 3, 0.0).err(),
        Some(Error::BufferTooSmall)
    );
    assert_eq!(
        attr.new_image_stride(px.clone(), 2, 4, 4, 0.0)
            .map(|i| (i.width(), i.height())),
        Ok((2, 4))
    );
    let rows: Vec<&[RGBA]> = px.chunks(4).take(3).collect();
    assert_eq!(attr.new_image_rows(rows, 4, 4, 0.0).err(), Some(Error::BufferTooSmall));
    assert_eq!(
        attr.new_image(px, usize::MAX, 2, 0.0).err(),
        Some(Error::ValueOutOfRange)
    );
    // same limit as libimagequant 2.4.1: width * height <= i32::MAX
    let fill = |row: &mut [RGBA], _| row.fill(RGBA::new(0, 0, 0, 255));
    assert_eq!(
        attr.new_image_fn(fill, 65536, 32768, 0.0).err(),
        Some(Error::ValueOutOfRange)
    );
    assert!(attr.new_image_fn(fill, 65535, 32768, 0.0).is_ok());
}

#[test]
fn all_pixel_sources_give_the_same_result() {
    let (w, h) = (45, 38);
    let pixels = photo_like(w, h, 12);
    let attr = libimgquant::new();
    let run = |mut image: libimgquant::Image<'_>| {
        let mut res = attr.quantize(&mut image).unwrap();
        res.set_dithering_level(1.0).unwrap();
        res.remapped(&mut image).unwrap()
    };

    let owned = run(attr.new_image(pixels.clone(), w, h, 0.0).unwrap());
    assert_eq!(owned, run(attr.new_image_borrowed(&pixels, w, h, 0.0).unwrap()));

    let rows: Vec<&[RGBA]> = pixels.chunks(w).collect();
    assert_eq!(owned, run(attr.new_image_rows(rows, w, h, 0.0).unwrap()));

    let stride = w + 5;
    let mut padded = vec![RGBA::new(255, 0, 0, 255); stride * h];
    for (dst, src) in padded.chunks_mut(stride).zip(pixels.chunks(w)) {
        dst[..w].copy_from_slice(src);
    }
    assert_eq!(
        owned,
        run(attr.new_image_stride_borrowed(&padded, w, h, stride, 0.0).unwrap())
    );
    assert_eq!(
        owned,
        run(attr.new_image_stride(padded.clone(), w, h, stride, 0.0).unwrap())
    );

    let calls = AtomicUsize::new(0);
    let callback = attr
        .new_image_fn(
            |row: &mut [RGBA], y: usize| {
                calls.fetch_add(1, Ordering::Relaxed);
                row.copy_from_slice(&pixels[y * w..(y + 1) * w]);
            },
            w,
            h,
            0.0,
        )
        .unwrap();
    assert_eq!(owned, run(callback));
    assert!(calls.load(Ordering::Relaxed) >= h);
}

#[test]
fn gamma_parameter() {
    let attr = libimgquant::new();
    let mut a = attr.new_image(gradient(30, 30), 30, 30, 0.0).unwrap();
    let mut b = attr.new_image(gradient(30, 30), 30, 30, 0.45455).unwrap();
    let mut c = attr.new_image(gradient(30, 30), 30, 30, 1.0).unwrap();
    let mut pa = attr.quantize(&mut a).unwrap();
    let mut pb = attr.quantize(&mut b).unwrap();
    let mut pc = attr.quantize(&mut c).unwrap();
    assert_eq!(pa.palette_vec(), pb.palette_vec(), "gamma 0 means 0.45455");
    assert_eq!(pc.output_gamma(), 1.0);
    let (_, idx) = pc.remapped(&mut c).unwrap();
    assert_eq!(idx.len(), 900);
}

#[test]
fn remap_buffers() {
    let attr = libimgquant::new();
    let (w, h) = (10, 6);
    let mut image = attr.new_image(gradient(w, h), w, h, 0.0).unwrap();
    let mut res = attr.quantize(&mut image).unwrap();

    let mut small = vec![0u8; w * h - 1];
    assert_eq!(res.remap_into(&mut image, &mut small), Err(Error::BufferTooSmall));

    let mut buf = vec![0u8; w * h + 3];
    res.remap_into(&mut image, &mut buf).unwrap();
    let (_, expected) = res.remapped(&mut image).unwrap();
    assert_eq!(&buf[..w * h], &expected[..]);

    let mut storage = vec![vec![0xEEu8; w + 2]; h];
    let mut rows: Vec<&mut [u8]> = storage.iter_mut().map(|r| &mut r[..]).collect();
    res.remap_rows(&mut image, &mut rows).unwrap();
    for (row, exp) in storage.iter().zip(expected.chunks(w)) {
        assert_eq!(&row[..w], exp);
        assert_eq!(&row[w..], &[0xEE, 0xEE], "padding untouched");
    }

    let mut short_rows: Vec<Vec<u8>> = vec![vec![0; w - 1]; h];
    let mut rows: Vec<&mut [u8]> = short_rows.iter_mut().map(|r| &mut r[..]).collect();
    assert_eq!(res.remap_rows(&mut image, &mut rows), Err(Error::BufferTooSmall));
}

#[test]
fn palette_after_remap_matches_indices() {
    let attr = libimgquant::new();
    let mut image = attr.new_image(photo_like(40, 40, 13), 40, 40, 0.0).unwrap();
    let mut res = attr.quantize(&mut image).unwrap();
    let mut buf = vec![0u8; 1600];
    res.remap_into(&mut image, &mut buf).unwrap();
    let max_index = usize::from(*buf.iter().max().unwrap());
    assert!(max_index < res.palette().len());
}

#[test]
fn log_callbacks() {
    let messages = Arc::new(Mutex::new(Vec::<String>::new()));
    let flushes = Arc::new(AtomicUsize::new(0));
    {
        let mut attr = libimgquant::new();
        let m = Arc::clone(&messages);
        attr.set_log_callback(move |_, msg| m.lock().unwrap().push(msg.to_owned()));
        let f = Arc::clone(&flushes);
        attr.set_log_flush_callback(move |_| {
            f.fetch_add(1, Ordering::Relaxed);
        });
        let mut image = attr.new_image(photo_like(50, 50, 14), 50, 50, 0.0).unwrap();
        attr.quantize(&mut image).unwrap();
        assert!(attr.new_image(gradient(2, 2), 0, 2, 0.0).is_err());
    }
    let messages = messages.lock().unwrap();
    assert!(
        messages.iter().any(|m| m.starts_with("  made histogram...")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.starts_with("  eliminated opaque tRNS-chunk entries"))
    );
    assert!(messages.iter().any(|m| m == "  error: width and height must be > 0"));
    assert_eq!(flushes.load(Ordering::Relaxed), 1, "flushed when dropped");
}

#[test]
fn types_are_thread_safe() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Attributes>();
    assert_send_sync::<libimgquant::Image<'static>>();
    assert_send_sync::<libimgquant::QuantizationResult>();
    assert_send_sync::<Error>();
}

#[test]
fn error_display() {
    let err: Box<dyn std::error::Error> = Box::new(Error::QualityTooLow);
    assert!(!err.to_string().is_empty());
}

#[test]
fn debug_output_is_short() {
    let attr = libimgquant::new();
    let mut image = attr.new_image(gradient(16, 8), 16, 8, 0.0).unwrap();
    assert_eq!(
        format!("{image:?}"),
        "Image { width: 16, height: 8, gamma: 0.45455, .. }"
    );
    let mut result = attr.quantize(&mut image).unwrap();
    result.remapped(&mut image).unwrap();
    let debug = format!("{result:?}");
    assert!(debug.starts_with("QuantizationResult { colors: "), "{debug}");
    assert!(
        debug.contains("profile: Improved, quantization_error: Some("),
        "{debug}"
    );
}
