use std::{env, fs, process};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        eprintln!("usage: svg2png <input.svg> <output.png> [size]");
        process::exit(2);
    }
    let size: u32 = args
        .get(3)
        .map(|s| s.parse().expect("size must be integer"))
        .unwrap_or(1024);

    let svg_data = fs::read(&args[1]).expect("failed to read input SVG");
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(&svg_data, &opt).expect("failed to parse SVG");

    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("failed to allocate pixmap");
    let svg_size = tree.size();
    let scale = (size as f32 / svg_size.width()).min(size as f32 / svg_size.height());
    let tx = (size as f32 - svg_size.width() * scale) * 0.5;
    let ty = (size as f32 - svg_size.height() * scale) * 0.5;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);

    resvg::render(&tree, transform, &mut pixmap.as_mut());
    pixmap.save_png(&args[2]).expect("failed to write PNG");
}
