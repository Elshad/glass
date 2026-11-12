// micprobe <feed> <mic>: plays a 440 Hz tone into the output whose name contains <feed>
// ("-": none) and records the input whose name contains <mic> for 3 s; prints the level
// and frequency heard ("recorded N frames; rms R; ~F Hz").
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let feed = args.get(1).cloned().unwrap_or("Glass".into());
    let mic = args.get(2).cloned().unwrap_or("Glass".into());
    let host = cpal::default_host();
    for d in host.output_devices().unwrap() { println!("output: {}", d.name().unwrap_or_default()); }
    for d in host.input_devices().unwrap() { println!("input: {}", d.name().unwrap_or_default()); }
    let play = feed != "-";
    let out = if play { host.output_devices().unwrap().find(|d| d.name().unwrap_or_default().contains(&feed)).expect("no feed output") } else { host.default_output_device().unwrap() };
    let inp = host.input_devices().unwrap().find(|d| d.name().unwrap_or_default().contains(&mic)).expect("no mic input");
    let oc = out.default_output_config().unwrap(); let ic = inp.default_input_config().unwrap();
    println!("feed {:?}\nmic {:?}", oc, ic);
    let rate = oc.sample_rate().0 as f32; let ch = oc.channels() as usize;
    let mut phase = 0f32;
    let ostream = out.build_output_stream(&oc.config(), move |data: &mut [f32], _| {
        for frame in data.chunks_mut(ch) { let v = (phase * 2.0 * std::f32::consts::PI).sin() * 0.5; phase = (phase + 440.0 / rate) % 1.0; for s in frame { *s = v; } }
    }, |e| eprintln!("out err {e}"), None).unwrap();
    let got: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let g = got.clone(); let ich = ic.channels() as usize;
    let istream = inp.build_input_stream(&ic.config(), move |data: &[f32], _| { let mut g = g.lock().unwrap(); for f in data.chunks(ich) { g.push(f[0]); } }, |e| eprintln!("in err {e}"), None).unwrap();
    if play { ostream.play().unwrap(); }
    istream.play().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(3));
    drop(istream); drop(ostream);
    let s = got.lock().unwrap(); let n = s.len(); let tail = &s[n / 2..];
    let rms = (tail.iter().map(|x| x * x).sum::<f32>() / tail.len().max(1) as f32).sqrt();
    let crossings = tail.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    let secs = tail.len() as f32 / ic.sample_rate().0 as f32;
    println!("recorded {n} frames; rms {rms:.3}; ~{:.0} Hz", crossings as f32 / secs.max(0.001));
}
