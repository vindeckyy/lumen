fn main() {
    for (w, h) in [(1280u32, 720u32), (1600, 900), (1920, 1080)] {
        let px = (w * h) as usize;
        let src = vec![90u8; px * 4];
        let mut dst = vec![0u8; px * 3 / 2];
        // Warm the pages.
        lumen_protocol::color::bgrx_to_nv12(&src, w, h, &mut dst).unwrap();
        let n = 20;
        let start = std::time::Instant::now();
        for _ in 0..n {
            lumen_protocol::color::bgrx_to_nv12(&src, w, h, &mut dst).unwrap();
        }
        let each = start.elapsed().as_secs_f64() / f64::from(n) * 1000.0;
        println!(
            "  {w}x{h}: the CPU conversion takes {each:.2} ms, so it alone caps the host at {:.0} fps",
            1000.0 / each
        );
    }
}
