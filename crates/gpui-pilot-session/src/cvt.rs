// A port of libxcvt 0.1.3's `libxcvt_gen_mode_info` with reduced = true, interlaced = false
// and no margins. libxcvt's notice follows.
//
// Copyright © 2000 Compaq Computer Corporation
// Copyright © 2002 Hewlett Packard Company
// Copyright © 2006 Intel Corporation
// Copyright © 2008, 2021 Red Hat, Inc.
//
// Permission to use, copy, modify, distribute, and sell this software and its
// documentation for any purpose is hereby granted without fee, provided that
// the above copyright notice appear in all copies and that both that copyright
// notice and this permission notice appear in supporting documentation, and
// that the name of the copyright holders not be used in advertising or
// publicity pertaining to distribution of the software without specific,
// written prior permission.  The copyright holders make no representations
// about the suitability of this software for any purpose.  It is provided "as
// is" without express or implied warranty.
//
// THE COPYRIGHT HOLDERS DISCLAIM ALL WARRANTIES WITH REGARD TO THIS SOFTWARE,
// INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS, IN NO
// EVENT SHALL THE COPYRIGHT HOLDERS BE LIABLE FOR ANY SPECIAL, INDIRECT OR
// CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE,
// DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
// TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE
// OF THIS SOFTWARE.

//! CVT reduced-blanking timings, as KWin builds a custom output mode from them.

/// One reduced-blanking mode as libxcvt generates it.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CvtMode {
    pub width: u32,
    pub height: u32,
    pub htotal: u32,
    pub vtotal: u32,
    pub hsync_start: u32,
    pub hsync_end: u32,
    pub vsync_start: u32,
    pub vsync_end: u32,
    pub dot_clock_khz: u64,
    /// KWin's `OutputModeline::custom()` rate: (dot_clock * 1e6 / htotal + vtotal / 2) / vtotal.
    pub refresh_mhz: u32,
}

/// The mode libxcvt 0.1.3 generates for `width` x `height` at `refresh_hz` with reduced
/// blanking, and the rate KWin derives from it.
pub fn cvt_reduced(width: u32, height: u32, refresh_hz: u32) -> CvtMode {
    const H_GRANULARITY: u32 = 8;
    const MIN_V_BPORCH: u32 = 6;
    const RB_MIN_VBLANK_US: f64 = 460.0;
    const RB_H_BLANK: u32 = 160;
    const RB_V_FPORCH: u32 = 3;
    const CLOCK_STEP_KHZ: u64 = 250;
    let hdisplay = width - width % H_GRANULARITY;
    // libxcvt picks the vsync width from the rounded hdisplay, not the requested width.
    let (w, h) = (hdisplay, height);
    let vsync = if h % 3 == 0 && h * 4 / 3 == w {
        4
    } else if h % 9 == 0 && h * 16 / 9 == w {
        5
    } else if h % 10 == 0 && h * 16 / 10 == w {
        6
    } else if (h % 4 == 0 && h * 5 / 4 == w) || (h % 9 == 0 && h * 15 / 9 == w) {
        7
    } else {
        10
    };
    // C: hperiod = (float)(1000000.0 / vfield_rate - 460.0) / vdisplay, in float.
    let hperiod =
        (1_000_000.0 / f64::from(refresh_hz as f32) - RB_MIN_VBLANK_US) as f32 / height as f32;
    // C: int vbilines = (float)460.0 / hperiod + 1;  float arithmetic, truncated.
    let vbilines = ((RB_MIN_VBLANK_US as f32 / hperiod) + 1.0) as u32;
    let vbilines = vbilines.max(RB_V_FPORCH + vsync + MIN_V_BPORCH);
    let vtotal = height + vbilines;
    let htotal = hdisplay + RB_H_BLANK;
    // C: dot_clock = htotal * 1000.0 / hperiod, in double, truncated to uint64 kHz.
    let mut dot_clock_khz = (f64::from(htotal) * 1000.0 / f64::from(hperiod)) as u64;
    dot_clock_khz -= dot_clock_khz % CLOCK_STEP_KHZ;
    // libxcvt's "FWXGA hack" keys on the rounded width: 1360..=1367 x 768 all become 1366.
    let fwxga = hdisplay == 1360 && height == 768;
    let width = if fwxga { 1366 } else { hdisplay };
    // The hack moves the sync pulse one pixel left of where the 1360 timings put it.
    let hsync_start = hdisplay + 48 - u32::from(fwxga);
    let vsync_start = height + RB_V_FPORCH;
    let (ht, vt) = (u64::from(htotal), u64::from(vtotal));
    let refresh_mhz = ((dot_clock_khz * 1_000_000 / ht + vt / 2) / vt) as u32;
    CvtMode {
        width,
        height,
        htotal,
        vtotal,
        hsync_start,
        hsync_end: hsync_start + 32,
        vsync_start,
        vsync_end: vsync_start + vsync,
        dot_clock_khz,
        refresh_mhz,
    }
}

#[cfg(test)]
mod tests {
    use super::cvt_reduced;

    #[test]
    fn sync_positions_match_the_cvt_tool() {
        // (w, h) -> (hsync_start, hsync_end, vsync_start, vsync_end) from `cvt -r`.
        let cases = [
            ((1280, 800), (1328, 1360, 803, 809)),
            ((1280, 720), (1328, 1360, 723, 728)),
            ((1368, 768), (1416, 1448, 771, 781)),
            ((1360, 768), (1407, 1439, 771, 781)),
        ];
        for ((w, h), want) in cases {
            let m = cvt_reduced(w, h, 144);
            let got = (m.hsync_start, m.hsync_end, m.vsync_start, m.vsync_end);
            assert_eq!(got, want, "{w}x{h}");
        }
    }

    #[test]
    fn reproduces_libxcvt_0_1_3_reduced_blanking() {
        // (w, h, Hz) -> (width, height, dot_clock_khz, htotal, vtotal, refresh_mhz), each read
        // from libxcvt 0.1.3 itself; the aspect ratios cover every vsync width (4, 5, 6, 7, 10).
        let cases = [
            ((1280, 800, 60), (1280, 800, 71_000, 1440, 823, 59_910)),
            ((1280, 800, 120), (1280, 800, 146_250, 1440, 847, 119_909)),
            ((1280, 800, 144), (1280, 800, 177_500, 1440, 857, 143_832)),
            ((1280, 800, 240), (1280, 800, 310_750, 1440, 900, 239_776)),
            (
                (1920, 1200, 144),
                (1920, 1200, 384_750, 2080, 1286, 143_838),
            ),
            (
                (1920, 1200, 240),
                (1920, 1200, 673_250, 2080, 1349, 239_939),
            ),
            (
                (1920, 1080, 144),
                (1920, 1080, 346_250, 2080, 1157, 143_878),
            ),
            (
                (2560, 1600, 144),
                (2560, 1600, 671_000, 2720, 1714, 143_927),
            ),
            ((1024, 768, 144), (1024, 768, 140_000, 1184, 823, 143_673)),
            (
                (1280, 1024, 144),
                (1280, 1024, 227_250, 1440, 1097, 143_858),
            ),
            ((1280, 768, 120), (1280, 768, 140_250, 1440, 813, 119_798)),
            ((800, 600, 240), (800, 600, 155_250, 960, 675, 239_583)),
            ((1366, 768, 144), (1366, 768, 180_000, 1520, 823, 143_889)),
            ((1360, 768, 144), (1366, 768, 180_000, 1520, 823, 143_889)),
            ((1367, 768, 144), (1366, 768, 180_000, 1520, 823, 143_889)),
            ((1368, 768, 144), (1368, 768, 180_750, 1528, 823, 143_733)),
            ((1283, 801, 144), (1280, 801, 177_750, 1440, 858, 143_867)),
            ((1100, 660, 144), (1096, 660, 127_750, 1256, 707, 143_864)),
        ];
        for ((w, h, hz), want) in cases {
            let m = cvt_reduced(w, h, hz);
            let got = (
                m.width,
                m.height,
                m.dot_clock_khz,
                m.htotal,
                m.vtotal,
                m.refresh_mhz,
            );
            assert_eq!(got, want, "{w}x{h}@{hz}");
        }
    }
}
