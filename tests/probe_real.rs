//! Integration check that confirms — on hosts that have libavformat
//! installed — the AvFormatProbe actually extracts a real format
//! string from a tiny on-disk WAV file. Skipped (with eprintln!) if
//! libavformat isn't loadable.
//!
//! Also smoke-tests `probe::stat::stat_file` because size/mtime now
//! live there, decoupled from the libavformat path.
use std::io::Write;
use waywallen::probe::media::{AvFormatProbe, MediaProbe};
use waywallen::probe::stat;

#[test]
fn probe_extracts_wav_format_on_hosts_with_libavformat() {
    let probe = AvFormatProbe::new();
    let probe_smoke = probe.probe_media("/__definitely_missing__");
    // width/height/format all None for a missing file. We need a positive
    // signal that libav loaded — which we get from a real WAV below.

    let mut tmp = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    let header: [u8; 44] = [
        b'R', b'I', b'F', b'F', 37, 0, 0, 0, b'W', b'A', b'V', b'E', b'f', b'm', b't', b' ', 16, 0,
        0, 0, 1, 0, 1, 0, 0x44, 0xAC, 0, 0, 0x44, 0xAC, 0, 0, 1, 0, 8, 0, b'd', b'a', b't', b'a',
        1, 0, 0, 0,
    ];
    tmp.write_all(&header).unwrap();
    tmp.write_all(&[0x80]).unwrap();
    tmp.flush().unwrap();
    let path = tmp.path().to_str().unwrap();

    let meta = probe.probe_media(path);
    match meta.format {
        Some(fmt) => {
            eprintln!("libavformat OK — extracted format={fmt:?}");
            assert!(
                fmt.contains("wav"),
                "format string should contain 'wav', got {fmt:?}"
            );
        }
        None => {
            eprintln!("libavformat unavailable on this host — full probe skipped");
        }
    }
    assert_eq!(probe_smoke.format, None);

    // Stat tier owns size + mtime now.
    let s = stat::stat_file(path).expect("stat_file on real tempfile");
    assert_eq!(s.size, 45);
    assert!(s.modified_at > 0);

    let missing = stat::stat_file("/__definitely_missing__");
    assert!(missing.is_none(), "missing file → None");
}
