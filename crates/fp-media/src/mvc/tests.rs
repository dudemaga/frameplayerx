use super::*;
use crate::decode::HwDecode;
use crate::info::read_info;
use fp_core::format::StereoLayout;
use fp_core::source::FileSource;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/mvc")
        .join(name)
}

fn open(path: &Path) -> Input {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    Input::open(
        Arc::new(FileSource::open(path).unwrap()),
        &name,
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap()
}

/// The two halves of a side-by-side I420 frame, each as packed planes.
fn views(frame: &Frame) -> [Vec<u8>; 2] {
    // SAFETY: a decoded 8-bit I420 frame.
    unsafe {
        let f = &*frame.as_ptr();
        assert_eq!(f.format, ff::AV_PIX_FMT_YUV420P);
        let mut eyes = [Vec::new(), Vec::new()];
        for p in 0..3 {
            let (w, h) = if p == 0 {
                (f.width as usize, f.height as usize)
            } else {
                (f.width as usize / 2, (f.height as usize).div_ceil(2))
            };
            for row in 0..h {
                let line =
                    std::slice::from_raw_parts(f.data[p].add(row * f.linesize[p] as usize), w);
                eyes[0].extend_from_slice(&line[..w / 2]);
                eyes[1].extend_from_slice(&line[w / 2..]);
            }
        }
        eyes
    }
}

/// Sends packets and hands every frame decoded, to the end, to `sink`.
fn drain(
    mut send: impl FnMut(*const ff::AVPacket) -> Result<bool>,
    mut receive: impl FnMut(&mut Frame) -> Result<Option<bool>>,
    packets: impl IntoIterator<Item = Packet>,
    mut sink: impl FnMut([Vec<u8>; 2]),
) {
    let mut frame = Frame::new();
    let mut take = |frame: &mut Frame, sink: &mut dyn FnMut([Vec<u8>; 2])| loop {
        match receive(frame).unwrap() {
            Some(true) => sink(views(frame)),
            Some(false) => return true,
            None => return false,
        }
    };
    for p in packets {
        while !send(p.as_ptr()).unwrap() {
            take(&mut frame, &mut sink);
        }
        take(&mut frame, &mut sink);
    }
    while !send(std::ptr::null()).unwrap() {
        take(&mut frame, &mut sink);
    }
    while !take(&mut frame, &mut sink) {}
}

/// Every frame a file decodes to (in software) through [`Decoder`].
fn decode_file(path: &Path) -> (String, Vec<[Vec<u8>; 2]>) {
    let mut frames = Vec::new();
    let name = decode_file_into(path, |f| frames.push(f));
    (name, frames)
}

fn decode_file_into(path: &Path, sink: impl FnMut([Vec<u8>; 2])) -> String {
    let mut input = open(path);
    let vi = input.best_stream(ff::AVMEDIA_TYPE_VIDEO).unwrap();
    let mut dec = Decoder::open(&input, vi, HwDecode::Off).unwrap();
    let mut packets = Vec::new();
    let mut pkt = Packet::new();
    while input.read(pkt.as_ptr()).unwrap() {
        if pkt.stream_index() == vi {
            packets.push(pkt.take());
        }
        pkt.unref();
    }
    let dec = std::cell::RefCell::new(&mut dec);
    drain(
        |p| dec.borrow_mut().send(p),
        |f| dec.borrow_mut().receive(f),
        packets,
        sink,
    );
    dec.borrow().name.clone()
}

/// Every frame an Annex B MVC stream decodes to, sent one NAL unit per
/// packet, and why it played as 2D if it did.
fn decode_annex_b(name: &str) -> (Option<String>, Vec<[Vec<u8>; 2]>) {
    decode_annex_b_file(&fixture(name))
}

fn decode_annex_b_file(path: &Path) -> (Option<String>, Vec<[Vec<u8>; 2]>) {
    let mut frames = Vec::new();
    let two_d = decode_annex_b_into(path, |f| frames.push(f));
    (two_d, frames)
}

fn decode_annex_b_into(path: &Path, sink: impl FnMut([Vec<u8>; 2])) -> Option<String> {
    let data = std::fs::read(path).unwrap();
    // SAFETY: fresh parameters for an H.264 decoder, freed after opening.
    let inner = unsafe {
        let mut p = ff::avcodec_parameters_alloc();
        (*p).codec_type = ff::AVMEDIA_TYPE_VIDEO;
        (*p).codec_id = ff::AV_CODEC_ID_H264;
        let d = Decoder::open_with(p, ff::AVRational { num: 1, den: 25 }, HwDecode::Off, false);
        ff::avcodec_parameters_free(&mut p);
        d.unwrap()
    };
    let config = Config {
        length_size: 0,
        base: Vec::new(),
        mvc: Vec::new(),
    };
    let mut mvc = MvcDecoder::new(inner, &config);
    let packets: Vec<Packet> = crate::nal::units(&data, 0)
        .enumerate()
        .map(|(i, nal)| {
            let pkt = Packet::new();
            // SAFETY: a fresh packet given a buffer, filled.
            unsafe {
                let p = pkt.as_ptr();
                ff::av_new_packet(p, nal.len() as i32 + 4);
                std::ptr::copy_nonoverlapping([0u8, 0, 0, 1].as_ptr(), (*p).data, 4);
                std::ptr::copy_nonoverlapping(nal.as_ptr(), (*p).data.add(4), nal.len());
                (*p).pts = i as i64;
                (*p).duration = 1;
                if nal[0] & 0x1f == 5 {
                    (*p).flags |= ff::AV_PKT_FLAG_KEY as i32;
                }
            }
            pkt
        })
        .collect();
    let mvc = std::cell::RefCell::new(&mut mvc);
    drain(
        |p| mvc.borrow_mut().send(p),
        |f| mvc.borrow_mut().receive(f),
        packets,
        sink,
    );
    mvc.borrow().two_d.clone()
}

/// The conformance manifest's hash of one view of every frame: 128-bit
/// FNV-1a as two 64-bit lanes.
fn view_hash(frames: &[[Vec<u8>; 2]], eye: usize) -> String {
    let (mut a, mut b) = (0xcbf2_9ce4_8422_2325u64, 0x8422_2325_cbf2_9ce4u64);
    for f in frames {
        for &byte in &f[eye] {
            a = (a ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
            b = (b ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
        }
    }
    format!("{a:016x}{b:016x}")
}

const INTERVIEW_COLOCATED: [&str; 2] = [
    "15e7b2bf2bd163e6011cedc71f42cf7b",
    "ebbe590d783b360c8d19e92c9cdf42b9",
];

#[test]
fn both_views_decode_as_the_reference_decoder_has_them() {
    for (name, frames, left, right) in [
        // CABAC, inter-view prediction (ITU conformance).
        (
            "MVCDS-4.264",
            9,
            "4b450245d8756f326571edeb914bb783",
            "3713ff32768c93a46ec0f09f175a4fc1",
        ),
        // CAVLC with I_PCM macroblocks, base view out of display order.
        (
            "mvc_display_ordering.264",
            8,
            "0b51b61ff4711325ac6af977efce9ce4",
            "95190a3f40631325987912b2f2731ce4",
        ),
    ] {
        let (two_d, decoded) = decode_annex_b(name);
        assert_eq!(two_d, None, "{name}");
        assert_eq!(decoded.len(), frames, "{name}");
        assert_eq!(view_hash(&decoded, 0), left, "{name}: left eye");
        assert_eq!(view_hash(&decoded, 1), right, "{name}: right eye");
    }
}

#[test]
fn what_the_rewrite_cannot_take_plays_its_left_eye_on_both_sides() {
    // Temporal direct prediction scales motion by picture order count
    // distances, which the merged stream doesn't keep.
    let (two_d, decoded) = decode_annex_b("mvc_base.264");
    assert!(two_d.unwrap().contains("temporal direct"));
    assert_eq!(decoded.len(), 2);
    let base = "db3681d9568edf2599f4c9f5e2cf4ce4";
    assert_eq!(view_hash(&decoded, 0), base);
    assert_eq!(view_hash(&decoded, 1), base);
}

#[test]
fn a_blu_ray_rip_opens_as_full_side_by_side_3d() {
    // As MakeMKV muxes it (mvcC box), and as mkvmerge does (none, and the
    // second view of each access unit in the next block).
    for name in ["interview_colocated.mkv", "interview_colocated_shifted.mkv"] {
        let input = open(&fixture(name));
        let info = read_info(&input);
        let v = info.video_stream().unwrap();
        assert!(v.mvc, "{name}");
        assert_eq!((v.width, v.height), (352, 144), "{name}");
        assert_eq!(info.hints.stereo, Some(StereoLayout::SideBySide), "{name}");
        assert!(!info.hints.eyes_swapped, "{name}");

        let (decoder, decoded) = decode_file(&fixture(name));
        assert_eq!(decoder, "h264 (3D Blu-ray)", "{name}");
        assert_eq!(decoded.len(), 9, "{name}");
        // The second view's B slices predict from the inter-view reference
        // as RefPicList1[0]: through the copy picture.
        assert_eq!(view_hash(&decoded, 0), INTERVIEW_COLOCATED[0], "{name}");
        assert_eq!(view_hash(&decoded, 1), INTERVIEW_COLOCATED[1], "{name}");
    }
}

#[test]
fn a_seek_starts_again_at_an_intra_picture() {
    let path = fixture("interview_colocated.mkv");
    let (_, all) = decode_file(&path);
    let mut input = open(&path);
    let vi = input.best_stream(ff::AVMEDIA_TYPE_VIDEO).unwrap();
    let mut dec = Decoder::open(&input, vi, HwDecode::Off).unwrap();
    let mut pkt = Packet::new();
    let mut frame = Frame::new();
    // Into the stream, then back to the start, as a seek does.
    for _ in 0..4 {
        assert!(input.read(pkt.as_ptr()).unwrap());
        dec.send(pkt.as_ptr()).unwrap();
        pkt.unref();
        while let Some(true) = dec.receive(&mut frame).unwrap() {}
    }
    input.seek(0.0).unwrap();
    dec.flush();
    let mut packets = Vec::new();
    while input.read(pkt.as_ptr()).unwrap() {
        packets.push(pkt.take());
    }
    let dec = std::cell::RefCell::new(&mut dec);
    let mut again = Vec::new();
    drain(
        |p| dec.borrow_mut().send(p),
        |f| dec.borrow_mut().receive(f),
        packets,
        |f| again.push(f),
    );
    assert_eq!(again.len(), all.len());
    assert!(again == all, "the same frames after the seek");
}

#[test]
fn configuration_records_with_an_mvcc_box() {
    let sps: &[u8] = &[0x67, 0x64, 0, 0x29, 0xac];
    let pps: &[u8] = &[0x68, 0xee, 0x38, 0x80];
    let subset: &[u8] = &[0x6f, 0x80, 0, 0x29, 0xac];
    let mut avcc = vec![1, 0x64, 0, 0x29, 0xff, 0xe1, 0, 5];
    avcc.extend_from_slice(sps);
    avcc.extend_from_slice(&[1, 0, 4]);
    avcc.extend_from_slice(pps);
    avcc.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
    assert!(config(ff::AV_CODEC_ID_H264, &avcc).is_none(), "no mvcC");
    let mut mvcc = vec![1, 0x80, 0, 0x29, 0xff, 2, 0, 5];
    mvcc.extend_from_slice(sps);
    mvcc.extend_from_slice(&[0, 5]);
    mvcc.extend_from_slice(subset);
    mvcc.extend_from_slice(&[1, 0, 4]);
    mvcc.extend_from_slice(pps);
    avcc.extend_from_slice(&(8 + mvcc.len() as u32).to_be_bytes());
    avcc.extend_from_slice(b"mvcC");
    avcc.extend_from_slice(&mvcc);
    let c = config(ff::AV_CODEC_ID_H264, &avcc).unwrap();
    assert_eq!(c.length_size, 4);
    assert_eq!(c.base, [sps, pps]);
    assert_eq!(c.mvc, [sps, subset, pps]);
    assert!(config(ff::AV_CODEC_ID_HEVC, &avcc).is_none());
    // Cut short: not MVC.
    assert!(config(ff::AV_CODEC_ID_H264, &avcc[..avcc.len() - 3]).is_none());
}

/// Decodes `$MVC_FILE` and writes each frame's eyes, left then right, as
/// raw I420 to `$MVC_OUT`, to compare with a reference MVC decoder's.
#[test]
#[ignore = "needs an MVC file: MVC_FILE=… MVC_OUT=…"]
fn decode_a_file_to_raw_views() {
    let path = std::env::var("MVC_FILE").expect("MVC_FILE");
    let mut out = std::fs::File::create(std::env::var("MVC_OUT").expect("MVC_OUT")).unwrap();
    let mut frames = 0;
    let sink = |[l, r]: [Vec<u8>; 2]| {
        out.write_all(&l).unwrap();
        out.write_all(&r).unwrap();
        frames += 1;
    };
    let name = if path.ends_with(".264") {
        let two_d = decode_annex_b_into(Path::new(&path), sink);
        format!("Annex B, 2D: {two_d:?}")
    } else {
        decode_file_into(Path::new(&path), sink)
    };
    eprintln!("{name}: {frames} frames");
}

/// Decodes every MVC stream of edge264-mvc's conformance suite
/// (`$EDGE264_CONFORMANCE`, its `tests/conformance` folder) and compares
/// both views with its manifest; streams the rewrite leaves to 2D are
/// listed, not failed.
#[test]
#[ignore = "needs edge264-mvc's conformance streams: EDGE264_CONFORMANCE=…"]
fn the_conformance_streams_decode_as_their_manifest_has_them() {
    let dir = PathBuf::from(std::env::var("EDGE264_CONFORMANCE").expect("EDGE264_CONFORMANCE"));
    let manifest = std::fs::read_to_string(dir.join("manifest.txt")).unwrap();
    let mut failed = Vec::new();
    for line in manifest.lines().filter(|l| l.starts_with("mvc")) {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (name, frames, left, right) = (f[0], f[1], f[3], f[4]);
        let path = dir.join(format!("{name}.264"));
        let (two_d, decoded) = decode_annex_b_file(&path);
        let got = (
            decoded.len().to_string(),
            view_hash(&decoded, 0),
            view_hash(&decoded, 1),
        );
        let ok = (got.0.as_str(), got.1.as_str(), got.2.as_str()) == (frames, left, right);
        eprintln!(
            "{name}: {}",
            match (&two_d, ok) {
                (Some(why), _) => format!("2D ({why})"),
                (None, true) => "both views exact".into(),
                (None, false) => format!("DIFFERS: {got:?}"),
            }
        );
        if two_d.is_none() && !ok {
            failed.push(name.to_string());
        }
    }
    assert!(failed.is_empty(), "{failed:?}");
}

#[test]
fn field_coding_is_read_from_the_parameter_sets() {
    let data = |input: &Input| {
        // SAFETY: the stream and its parameters live as long as the input.
        let par = unsafe { &*(*input.streams()[0]).codecpar };
        extradata(par).to_vec()
    };
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let interlaced = data(&open(&tests.join("interlaced_h264.mp4")));
    assert!(field_coded(&interlaced));
    let (_, sets) = record_parameter_sets(&interlaced, 0x1f).unwrap();
    let annex_b: Vec<u8> = sets
        .iter()
        .flat_map(|s| [&[0, 0, 0, 1], *s].concat())
        .collect();
    assert!(field_coded(&annex_b));
    assert!(!field_coded(&data(&open(
        &tests.join("h264_aac_180_LR.mp4")
    ))));
    assert!(!field_coded(&[]));
}
