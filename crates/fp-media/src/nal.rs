//! Just enough of the H.264 and HEVC bitstreams to find pictures no other
//! picture is decoded from, which a precise seek can leave out on its way
//! to the target.

use crate::mvc::bits::{Reader, unescape};
use fp_core::format::StereoLayout;
use fp_ffmpeg_sys as ff;

/// H.264's SEI NAL unit type, HEVC's prefix SEI, and the SEI message that says
/// how a picture holds two eyes.
const H264_SEI: u8 = 6;
const HEVC_SEI: u8 = 39;
const FRAME_PACKING: usize = 45;

/// Reads the NAL units in one stream's packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NalReader {
    hevc: bool,
    /// Bytes in each NAL unit's length prefix (MP4, Matroska), or 0 for
    /// Annex B start codes (MPEG-TS).
    length_size: usize,
    /// HEVC's highest temporal sub-layer, once an SPS gave it. A
    /// non-reference picture below it can still be referenced by the
    /// layers above.
    top_layer: Option<u8>,
}

impl NalReader {
    /// A reader for an H.264 or HEVC stream with this extradata.
    pub(crate) fn new(codec: ff::AVCodecID, extradata: &[u8]) -> Option<NalReader> {
        let hevc = match codec {
            ff::AV_CODEC_ID_H264 => false,
            ff::AV_CODEC_ID_HEVC => true,
            _ => return None,
        };
        let mut r = NalReader {
            hevc,
            length_size: 0,
            top_layer: None,
        };
        match extradata {
            // hvcC (FFmpeg's test: Annex B starts 00 00 01 or 00 00 00 01).
            [a, b, c, ..] if hevc && extradata.len() >= 23 && (*a != 0 || *b != 0 || *c > 1) => {
                r.length_size = (extradata[21] & 3) as usize + 1;
                r.read_hvcc_arrays(&extradata[22..]);
            }
            [1, _, _, _, size, ..] if !hevc => r.length_size = (size & 3) as usize + 1,
            _ => {
                for nal in units(extradata, 0) {
                    r.note_sps(nal);
                }
            }
        }
        Some(r)
    }

    fn read_hvcc_arrays(&mut self, data: &[u8]) {
        let Some((&arrays, mut rest)) = data.split_first() else {
            return;
        };
        for _ in 0..arrays {
            let [_, hi, lo, tail @ ..] = rest else {
                return;
            };
            rest = tail;
            for _ in 0..u16::from_be_bytes([*hi, *lo]) {
                let [hi, lo, tail @ ..] = rest else {
                    return;
                };
                let len = u16::from_be_bytes([*hi, *lo]) as usize;
                let Some(nal) = tail.get(..len) else {
                    return;
                };
                self.note_sps(nal);
                rest = &tail[len..];
            }
        }
    }

    fn note_sps(&mut self, nal: &[u8]) {
        // sps_max_sub_layers_minus1 follows the 4-bit VPS id; layers above
        // the base code it differently.
        if let [h0, h1, b, ..] = *nal
            && self.hevc
            && (h0 >> 1) & 0x3f == 33
            && h0 & 1 == 0
            && h1 >> 3 == 0
        {
            let top = (b >> 1) & 7;
            self.top_layer = Some(self.top_layer.map_or(top, |t| t.max(top)));
        }
    }

    pub(crate) fn length_size(&self) -> usize {
        self.length_size
    }

    /// The 3D layout a packet's frame packing SEI message states, and whether
    /// the right eye comes first. None when it has none, or states a layout
    /// that can't be shown.
    pub(crate) fn frame_packing(&self, data: &[u8]) -> Option<(StereoLayout, bool)> {
        units(data, self.length_size).find_map(|nal| {
            let h0 = *nal.first()?;
            let (kind, sei, header) = if self.hevc {
                ((h0 >> 1) & 0x3f, HEVC_SEI, 2)
            } else {
                (h0 & 0x1f, H264_SEI, 1)
            };
            if kind != sei {
                return None;
            }
            sei_frame_packing(&unescape(nal.get(header..)?))
        })
    }

    /// Whether a packet holds only pictures no other picture is decoded
    /// from, and no parameter sets. SPSs on the way are noted.
    pub(crate) fn disposable(&mut self, data: &[u8]) -> bool {
        let (mut pictures, mut parameter_sets) = (0, false);
        for nal in units(data, self.length_size) {
            let Some(&h0) = nal.first() else {
                continue;
            };
            let unreferenced = if self.hevc {
                let Some(&h1) = nal.get(1) else {
                    continue;
                };
                let kind = (h0 >> 1) & 0x3f;
                if (32..=34).contains(&kind) {
                    self.note_sps(nal);
                    parameter_sets = true;
                    continue;
                }
                if kind >= 32 {
                    continue;
                }
                // TRAIL_N, TSA_N, STSA_N, RADL_N, RASL_N and the reserved
                // non-reference types are the even ones up to 14.
                let temporal_id = (h1 & 7).saturating_sub(1);
                kind <= 14 && kind % 2 == 0 && self.top_layer.is_some_and(|t| temporal_id >= t)
            } else {
                match h0 & 0x1f {
                    7 | 8 | 13 | 15 => {
                        parameter_sets = true;
                        continue;
                    }
                    1..=5 => h0 & 0x60 == 0,
                    _ => continue,
                }
            };
            if !unreferenced {
                return false;
            }
            pictures += 1;
        }
        pictures > 0 && !parameter_sets
    }
}

/// Finds the frame packing message among an SEI RBSP's messages.
fn sei_frame_packing(mut rbsp: &[u8]) -> Option<(StereoLayout, bool)> {
    // Each is its type and size, in bytes of 255 and a last one that add up.
    fn number(rest: &mut &[u8]) -> Option<usize> {
        let mut n = 0;
        loop {
            let (&byte, tail) = rest.split_first()?;
            *rest = tail;
            n += byte as usize;
            if byte != 255 {
                return Some(n);
            }
        }
    }
    loop {
        let kind = number(&mut rbsp)?;
        let size = number(&mut rbsp)?;
        let payload = rbsp.get(..size)?;
        if kind == FRAME_PACKING {
            return frame_packing_message(payload);
        }
        rbsp = &rbsp[size..];
    }
}

fn frame_packing_message(payload: &[u8]) -> Option<(StereoLayout, bool)> {
    let mut r = Reader::new(payload, 0);
    r.ue().ok()?; // frame_packing_arrangement_id
    if r.flag().ok()? {
        return None; // cancelled
    }
    let arrangement = r.u(7).ok()?;
    r.flag().ok()?; // quincunx_sampling_flag
    let interpretation = r.u(6).ok()?;
    let layout = match arrangement {
        3 => StereoLayout::SideBySide,
        4 => StereoLayout::TopBottom,
        6 => StereoLayout::Mono,
        _ => return None,
    };
    Some((layout, layout != StereoLayout::Mono && interpretation == 2))
}

/// The NAL units in `data`, each from its header on. A unit cut short by
/// the end of the data is returned as far as it goes.
pub(crate) fn units(data: &[u8], length_size: usize) -> impl Iterator<Item = &[u8]> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if length_size > 0 {
            let len = rest
                .get(..length_size)?
                .iter()
                .fold(0usize, |n, &b| n << 8 | b as usize);
            let tail = &rest[length_size..];
            let unit = tail.get(..len).unwrap_or(tail);
            rest = &tail[unit.len()..];
            Some(unit)
        } else {
            let tail = &rest[start_code(rest)? + 3..];
            let unit = &tail[..start_code(tail).unwrap_or(tail.len())];
            rest = &tail[unit.len()..];
            Some(unit)
        }
    })
}

pub(crate) fn start_code(data: &[u8]) -> Option<usize> {
    data.windows(3).position(|w| w == [0, 0, 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_packing_message_says_the_3d_layout() {
        let h264 = NalReader {
            hevc: false,
            length_size: 0,
            top_layer: None,
        };
        let hevc = NalReader { hevc: true, ..h264 };
        // id 0, not cancelled, then the arrangement, sampling and eye order.
        let sei = |payload: [u8; 2]| vec![45, 2, payload[0], payload[1], 0x80];
        let side_by_side = sei([0x81, 0x81]);
        let cases = [
            (
                side_by_side.clone(),
                Some((StereoLayout::SideBySide, false)),
            ),
            (sei([0x81, 0x82]), Some((StereoLayout::SideBySide, true))),
            (sei([0x82, 0x01]), Some((StereoLayout::TopBottom, false))),
            (sei([0x83, 0x01]), Some((StereoLayout::Mono, false))),
            (sei([0x80, 0x81]), None),       // checkerboard
            (sei([0x82, 0x81]), None),       // frame sequential
            (vec![45, 1, 0xc0, 0x80], None), // cancelled
            (vec![45, 9, 0x81], None),       // cut short
        ];
        for (rbsp, want) in cases {
            let in_h264 = [&[6u8][..], &rbsp].concat();
            assert_eq!(h264.frame_packing(&annex_b(&[&in_h264])), want, "{rbsp:x?}");
            let in_hevc = [&[39u8 << 1, 1][..], &rbsp].concat();
            assert_eq!(hevc.frame_packing(&annex_b(&[&in_hevc])), want, "{rbsp:x?}");
        }

        // After other messages and other units, in a length-prefixed packet.
        let two = [&[6u8, 5, 3, 1, 2, 3][..], &side_by_side].concat();
        let with_lengths = NalReader {
            length_size: 4,
            ..h264
        };
        assert_eq!(
            with_lengths.frame_packing(&prefixed(&[&[0x65, 1, 2], &two])),
            Some((StereoLayout::SideBySide, false))
        );
        // Not an SEI unit, and another message type with a long type number.
        assert_eq!(
            h264.frame_packing(&annex_b(&[&[0x65, 45, 2, 0x81, 0x81]])),
            None
        );
        assert_eq!(h264.frame_packing(&annex_b(&[&[6, 255, 1, 0, 0x80]])), None);
    }

    fn prefixed(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&(n.len() as u32).to_be_bytes());
            out.extend_from_slice(n);
        }
        out
    }

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }

    /// An hvcC holding one SPS with `max_sub_layers_minus1`.
    fn hvcc(max_sub_layers_minus1: u8) -> Vec<u8> {
        let mut c = vec![1u8; 22];
        c[21] = 0xf0 | 3; // 4-byte lengths
        c.push(1); // arrays
        c.extend_from_slice(&[0x80 | 33, 0, 1, 0, 4]);
        c.extend_from_slice(&[33 << 1, 1, max_sub_layers_minus1 << 1, 0]);
        c
    }

    const AVCC: [u8; 7] = [1, 0x64, 0, 0x28, 0xff, 0xe0, 0];

    #[test]
    fn h264_non_reference_slices() {
        let mut r = NalReader::new(ff::AV_CODEC_ID_H264, &AVCC).unwrap();
        assert!(r.disposable(&prefixed(&[&[0x01, 0x9a]])));
        // With an SEI ahead, and a picture in several slices.
        assert!(r.disposable(&prefixed(&[&[0x06, 5], &[0x01, 1], &[0x01, 2]])));
        assert!(!r.disposable(&prefixed(&[&[0x41, 0x9a]])), "referenced");
        assert!(!r.disposable(&prefixed(&[&[0x65, 0x88]])), "IDR");
        assert!(
            !r.disposable(&prefixed(&[&[0x67, 0x64], &[0x01, 0x9a]])),
            "SPS"
        );
        assert!(!r.disposable(&prefixed(&[&[0x01, 0x9a], &[0x21, 0x9a]])));
        assert!(!r.disposable(&prefixed(&[&[0x06, 5]])), "no picture");
        assert!(!r.disposable(&[]));
    }

    #[test]
    fn h264_annex_b() {
        let mut r = NalReader::new(ff::AV_CODEC_ID_H264, &[]).unwrap();
        assert!(r.disposable(&annex_b(&[&[0x09, 0x10], &[0x01, 0x9a, 0, 0, 3, 1]])));
        assert!(!r.disposable(&annex_b(&[&[0x09, 0x10], &[0x41, 0x9a]])));
        // Three-byte start codes.
        assert!(r.disposable(&[0, 0, 1, 0x01, 0x9a, 0, 0, 1, 0x01, 0x9b]));
    }

    #[test]
    fn hevc_non_reference_pictures_of_the_top_layer() {
        let mut r = NalReader::new(ff::AV_CODEC_ID_HEVC, &hvcc(0)).unwrap();
        let trail_n: &[u8] = &[0 << 1, 1, 0xaf];
        let trail_r: &[u8] = &[1 << 1, 1, 0xaf];
        let rasl_n: &[u8] = &[8 << 1, 1, 0xaf];
        let sei: &[u8] = &[39 << 1, 1, 5];
        assert!(r.disposable(&prefixed(&[sei, trail_n, trail_n])));
        assert!(r.disposable(&prefixed(&[rasl_n])));
        assert!(!r.disposable(&prefixed(&[trail_r])));
        assert!(!r.disposable(&prefixed(&[&[19 << 1, 1, 0xaf]])), "IDR");
        assert!(
            !r.disposable(&prefixed(&[&[34 << 1, 1, 0xc1], trail_n])),
            "PPS"
        );

        // Three sub-layers: only the top one's are unused by the others.
        let mut r = NalReader::new(ff::AV_CODEC_ID_HEVC, &hvcc(2)).unwrap();
        assert!(!r.disposable(&prefixed(&[trail_n])));
        assert!(!r.disposable(&prefixed(&[&[0, 2, 0xaf]])));
        assert!(r.disposable(&prefixed(&[&[0, 3, 0xaf]])));
    }

    #[test]
    fn hevc_parameter_sets_in_band_wait_for_an_sps() {
        let trail_n: &[u8] = &[0, 1, 0xaf];
        // A keyframe as hev1 MP4s and MPEG-TS carry it: AUD, VPS, SPS, PPS.
        let key: [&[u8]; 5] = [
            &[35 << 1, 1, 0x10],
            &[32 << 1, 1, 0x0c],
            &[33 << 1, 1, 0x01],
            &[34 << 1, 1, 0xc1],
            &[19 << 1, 1, 0xaf],
        ];
        let hev1 = [
            1u8, 1, 0x60, 0, 0, 0, 0xb0, 0, 0, 0, 0, 0, 0xb7, 0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8,
            0x3b, 0xf0, 0x8b, 0,
        ];
        for (mut r, pack) in [
            (
                NalReader::new(ff::AV_CODEC_ID_HEVC, &[]).unwrap(),
                annex_b as fn(&[&[u8]]) -> Vec<u8>,
            ),
            (
                NalReader::new(ff::AV_CODEC_ID_HEVC, &hev1).unwrap(),
                prefixed,
            ),
        ] {
            assert!(!r.disposable(&pack(&[trail_n])), "layers unknown");
            assert!(!r.disposable(&pack(&key)));
            assert!(r.disposable(&pack(&[trail_n])));
        }
    }

    #[test]
    fn other_codecs_and_cut_short_data() {
        assert!(NalReader::new(ff::AV_CODEC_ID_VP9, &[]).is_none());
        let mut r = NalReader::new(ff::AV_CODEC_ID_H264, &AVCC).unwrap();
        // A length running past the end: the unit as far as it goes.
        assert!(r.disposable(&[0, 0, 0, 9, 0x01, 0x9a]));
        assert!(!r.disposable(&[0, 0, 0]));
        assert_eq!(units(&[0, 0, 0, 0, 0, 0, 1], 0).count(), 1);
    }
}
