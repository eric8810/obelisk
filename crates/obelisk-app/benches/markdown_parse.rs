// Bench: markdown parse cost per timeline message (parity #49 evidence).
// Measures pulldown-cmark parse + a trivial block walk over a typical
// message body, the exact work fc-ui's Markdown::render repeats per frame.
use std::time::Instant;

fn typical_message() -> String {
    let mut text = String::from("Here is the plan for this change:\n\n");
    for i in 0..6 {
        text.push_str(&format!(
            "{i}. Step **{}** with `code_{i}` and a [link](https://example.com/{i})\n",
            format_args!("number-{}", i * 7)
        ));
    }
    text.push_str("\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\n");
    text.push_str(
        "Tail paragraph with *emphasis* and more text to give the parser a realistic body. ",
    );
    text.push_str("The quick brown fox jumps over the lazy dog repeatedly to add length. ");
    text.push_str("End of message.\n");
    text
}

fn main() {
    let source = typical_message();
    println!("message size: {} bytes", source.len());

    let options = pulldown_cmark::Options::empty() | pulldown_cmark::Options::ENABLE_TABLES;
    let iterations = 5_000;

    // Warmup.
    for _ in 0..100 {
        let parser = pulldown_cmark::Parser::new_ext(&source, options);
        let events: Vec<pulldown_cmark::Event> = parser.collect();
        std::hint::black_box(events.len());
    }

    let start = Instant::now();
    for _ in 0..iterations {
        let parser = pulldown_cmark::Parser::new_ext(&source, options);
        let events: Vec<pulldown_cmark::Event> = parser.collect();
        std::hint::black_box(events.len());
    }
    let per_call = start.elapsed().as_micros() as f64 / iterations as f64;
    println!("parse cost: {per_call:.1} µs/message (avg of {iterations})");
    println!(
        "visible items ~15 → {:.2} ms/frame worst case",
        per_call * 15.0 / 1000.0
    );
}
