//! `OpenCV` owns the published `YuNet` postprocessing and five-landmark `SFace` alignment.
use super::{
    Config, Detected, Engine, Feature, Frame, HEIGHT, JpegEncoder, WIDTH, select_largest_box,
};
use anyhow::{Result, ensure};
use opencv::{
    core::{self, Mat, Ptr, Size, Vector},
    imgcodecs, imgproc,
    objdetect::{FaceDetectorYN, FaceRecognizerSF},
    prelude::*,
};

pub(super) struct OpenCv {
    detector: Ptr<FaceDetectorYN>,
    recognizer: Option<Ptr<FaceRecognizerSF>>,
}

/// Stateless I420-to-JPEG encoder kept outside the detector/recognizer mutex.
pub(super) struct OpenCvJpeg;

impl JpegEncoder for OpenCvJpeg {
    fn encode(&self, frame: &Frame) -> Result<Vec<u8>> {
        encode_jpeg(frame)
    }
}

impl OpenCv {
    pub(super) fn open(cfg: &Config) -> Result<Self> {
        core::set_num_threads(2)?;
        Ok(Self {
            detector: FaceDetectorYN::create(
                &cfg.yunet,
                "",
                Size::new(i32::from(WIDTH), i32::from(HEIGHT)),
                0.8,
                0.3,
                50,
                0,
                0,
            )?,
            recognizer: cfg
                .sface
                .as_deref()
                .map(|p| FaceRecognizerSF::create(p, "", 0, 0))
                .transpose()?,
        })
    }
}

fn bgr(frame: &Frame) -> Result<Mat> {
    let raw = Mat::from_slice(frame.pixels.as_ref())?;
    let yuv = raw.reshape(1, i32::from(HEIGHT) * 3 / 2)?;
    let mut bgr = Mat::default();
    imgproc::cvt_color_def(&yuv, &mut bgr, imgproc::COLOR_YUV2BGR_I420)?;
    Ok(bgr)
}

fn encode_jpeg(frame: &Frame) -> Result<Vec<u8>> {
    let mut out = Vector::<u8>::new();
    ensure!(
        imgcodecs::imencode(
            ".jpg",
            &bgr(frame)?,
            &mut out,
            &Vector::from_slice(&[imgcodecs::IMWRITE_JPEG_QUALITY, 80])
        )?,
        "JPEG encoding failed"
    );
    Ok(out.to_vec())
}

impl Engine for OpenCv {
    fn detect(&mut self, frame: &Frame, extract: bool) -> Result<Detected> {
        let image = bgr(frame)?;
        let mut faces = Mat::default();
        self.detector.detect(&image, &mut faces)?;
        let rows = usize::try_from(faces.rows())?;
        if rows > 0 {
            ensure!(faces.cols() >= 4, "unexpected face detector columns");
        }
        let mut raw_boxes: Vec<[f32; 4]> = Vec::with_capacity(rows);
        for row in 0..faces.rows() {
            raw_boxes.push([
                *faces.at_2d::<f32>(row, 0)?,
                *faces.at_2d::<f32>(row, 1)?,
                *faces.at_2d::<f32>(row, 2)?,
                *faces.at_2d::<f32>(row, 3)?,
            ]);
        }
        let (count, selected) = select_largest_box(raw_boxes);
        let (best, largest) = if let Some((index, rect)) = selected {
            (i32::try_from(index)?, Some(rect))
        } else {
            (0, None)
        };
        let feature = if let (true, Some(rec)) =
            (extract && largest.is_some(), self.recognizer.as_mut())
        {
            let landmarks_ok = faces.cols() >= 14
                && (4..14).all(|col| faces.at_2d::<f32>(best, col).is_ok_and(|v| v.is_finite()));
            if landmarks_ok {
                let mut aligned = Mat::default();
                rec.align_crop(&image, &faces.row(best)?, &mut aligned)?;
                let mut values = Mat::default();
                rec.feature(&aligned, &mut values)?;
                Some(Feature::new(values.data_typed::<f32>()?)?)
            } else {
                None
            }
        } else {
            None
        };
        Ok(Detected {
            count,
            largest,
            feature,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::{FRAME_BYTES, Gallery};
    use std::{env, fs::File, io::Read, time::Instant};

    #[test]
    fn packed_i420_converts_to_full_size_decodable_jpeg() {
        let mut pixels = vec![128u8; FRAME_BYTES];
        pixels[..usize::from(WIDTH) * usize::from(HEIGHT)].fill(16);
        let frame = Frame {
            captured: Instant::now(),
            pixels: pixels.into(),
        };
        let color = bgr(&frame).unwrap();
        assert_eq!(color.at_2d::<core::Vec3b>(0, 0).unwrap().0, [0, 0, 0]);
        let jpeg = encode_jpeg(&frame).unwrap();
        let decoded =
            imgcodecs::imdecode(&Vector::from_slice(&jpeg), imgcodecs::IMREAD_COLOR).unwrap();
        assert_eq!(
            decoded.size().unwrap(),
            Size::new(i32::from(WIDTH), i32::from(HEIGHT))
        );
    }

    #[test]
    #[ignore = "requires operator-owned models and three labeled local I420 frames; see vision/README.md"]
    fn enrollment_matches_another_view_but_not_an_unknown_person() -> Result<()> {
        let cfg = Config::from_env()?.ok_or_else(|| anyhow::anyhow!("set BUDDY_VISION=1"))?;
        let mut engine = OpenCv::open(&cfg)?;
        let mut feature = |key| -> Result<Feature> {
            let mut bytes = Vec::new();
            File::open(env::var(key)?)?
                .take(FRAME_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() == FRAME_BYTES, "expected one packed I420 frame");
            let detected = engine.detect(
                &Frame {
                    captured: Instant::now(),
                    pixels: bytes.into(),
                },
                true,
            )?;
            ensure!(detected.count == 1, "fixture must show exactly one face");
            detected
                .feature
                .ok_or_else(|| anyhow::anyhow!("SFace feature missing"))
        };
        let path = crate::vision::tests::Scratch::new();
        let mut g = Gallery::open(&path.0)?;
        g.add("fixture", feature("BUDDY_FIXTURE_ENROLL")?)?;
        assert_eq!(
            g.identify(&feature("BUDDY_FIXTURE_KNOWN")?).as_deref(),
            Some("fixture")
        );
        assert!(g.identify(&feature("BUDDY_FIXTURE_UNKNOWN")?).is_none());
        Ok(())
    }
}
