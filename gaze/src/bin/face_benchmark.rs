// SPDX-FileCopyrightText: 2026 Gundu Labs
// SPDX-License-Identifier: GPL-3.0-or-later

//! Standalone accuracy check: runs gaze's real detect/align/recognize pipeline over two
//! directories of still photos and scores each against the user's already-enrolled RGB
//! templates, using whatever security level/threshold `/etc/gaze/config.toml` has set.
//! Needs root (same as gazed) to read `/var/lib/gaze` and unseal the TPM-protected key.

// These are the daemon's own modules, included wholesale. Only the read paths are exercised
// here, so the enrolment/eviction half of each is legitimately dead in this binary.
#[allow(dead_code)]
#[path = "../align.rs"]
mod align;
#[allow(dead_code)]
#[path = "../crypto.rs"]
mod crypto;
#[allow(dead_code)]
#[path = "../models.rs"]
mod models;
#[allow(dead_code)]
#[path = "../recognize.rs"]
mod recognize;
#[allow(dead_code)]
#[path = "../tpm.rs"]
mod tpm;
#[allow(dead_code)]
#[path = "../users.rs"]
mod users;

use gaze_core::config::{Config, MODELS_DIR, USERS_DIR};
use gaze_core::detect::FaceDetector;
use gaze_core::face::Spectrum;
use recognize::FaceRecognizer;
use std::path::{Path, PathBuf};
use users::UserDatabase;

struct Outcome {
    file_name: String,
    score: f32,
    is_match: bool,
}

fn collect_images(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let is_image = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .is_some_and(|e| matches!(e.as_str(), "jpg" | "jpeg" | "png" | "bmp"));
        if is_image {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// The loaded pipeline plus everything a score is measured against.
struct Bench {
    detector: FaceDetector,
    recognizer: FaceRecognizer,
    db: UserDatabase,
    username: String,
    threshold: f32,
}

impl Bench {
    fn score_image(&mut self, path: &Path) -> anyhow::Result<Outcome> {
        score_image(
            path,
            &mut self.detector,
            &mut self.recognizer,
            &self.db,
            &self.username,
            self.threshold,
        )
    }

    fn run_dir(
        &mut self,
        label: &str,
        dir: &Path,
        expect_match: bool,
    ) -> anyhow::Result<(usize, usize)> {
        let images = collect_images(dir)?;
        println!(
            "\n=== {label}: {} ({} images) ===",
            dir.display(),
            images.len()
        );

        let mut correct = 0;
        for path in &images {
            match self.score_image(path) {
                Ok(outcome) => {
                    let verdict = if outcome.is_match { "MATCH" } else { "reject" };
                    let ok = outcome.is_match == expect_match;
                    if ok {
                        correct += 1;
                    }
                    println!(
                        "  {:<40} score={:.4} threshold={:.2} -> {:<6} {}",
                        outcome.file_name,
                        outcome.score,
                        self.threshold,
                        verdict,
                        if ok { "OK" } else { "WRONG" }
                    );
                }
                Err(e) => println!(
                    "  {:<40} ERROR: {e}",
                    path.file_name().unwrap().to_string_lossy()
                ),
            }
        }
        Ok((correct, images.len()))
    }
}

fn score_image(
    path: &Path,
    detector: &mut FaceDetector,
    recognizer: &mut FaceRecognizer,
    db: &UserDatabase,
    username: &str,
    threshold: f32,
) -> anyhow::Result<Outcome> {
    // opencv is built without the imgcodecs module here (see scripts/opencv-headers.sh), so
    // decode with the `image` crate and hand the detector a BGR Mat built directly over the
    // buffer, the same way camera.rs wraps a GStreamer BGR frame.
    let rgb = image::open(path)?.to_rgb8();
    let (width, height) = rgb.dimensions();
    let mut bgr = vec![0u8; (width * height * 3) as usize];
    for (i, pixel) in rgb.pixels().enumerate() {
        bgr[i * 3] = pixel[2];
        bgr[i * 3 + 1] = pixel[1];
        bgr[i * 3 + 2] = pixel[0];
    }
    let stride = (width * 3) as usize;
    let mat = unsafe {
        opencv::core::Mat::new_rows_cols_with_data_unsafe(
            height as i32,
            width as i32,
            opencv::core::CV_8UC3,
            bgr.as_mut_ptr() as *mut std::ffi::c_void,
            stride,
        )?
    };

    let (_bboxes, kpss, mat_rgb) = detector
        .detect(&mat)
        .map_err(|e| anyhow::anyhow!("detect: {e}"))?;
    let kpss = kpss.ok_or_else(|| anyhow::anyhow!("detector returned no landmarks"))?;
    // Row 0 is the highest-confidence face after NMS (see FaceDetector::detect).
    let aligned = align::align_face(&mat_rgb, &kpss, 0)?;
    let embedding = recognizer.get_embedding(&aligned)?;

    let results = db.match_faces(username, &embedding, threshold, Spectrum::Rgb)?;
    let (score, is_match) = results
        .into_iter()
        .next()
        .map(|(_, score, _, is_match, _)| (score, is_match))
        .unwrap_or((0.0, false));

    Ok(Outcome {
        file_name: path.file_name().unwrap().to_string_lossy().into_owned(),
        score,
        is_match,
    })
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: {} <dir-of-you> <dir-of-others> [username]",
            args.first().map(String::as_str).unwrap_or("face-benchmark")
        );
        std::process::exit(2);
    }
    let dir_me = PathBuf::from(&args[1]);
    let dir_notme = PathBuf::from(&args[2]);
    let username = args.get(3).cloned().unwrap_or_else(|| {
        std::env::var("SUDO_USER")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "root".to_string())
    });

    let config = Config::load()?;
    let security = &config.security;
    let threshold = security.rgb_threshold();

    println!(
        "username={username} detector={} recognizer={} rgb_threshold={threshold:.2}",
        security.detector(),
        security.recognizer()
    );

    let (det_path, rec_path) =
        models::ensure_models(MODELS_DIR, security.detector(), security.recognizer())?;
    let detector = FaceDetector::new_with_inference(det_path.to_str().unwrap(), &config.inference)
        .map_err(|e| anyhow::anyhow!("failed to load detector: {e}"))?;
    let recognizer =
        FaceRecognizer::new_with_inference(rec_path.to_str().unwrap(), &config.inference)?;

    let cipher = if config.storage.encrypt_templates {
        let dek = tpm::load_or_create_dek(Path::new(tpm::STATE_DIR))?;
        Some(crypto::EmbeddingCipher::new(&dek))
    } else {
        None
    };
    let db =
        UserDatabase::new_with_cipher(USERS_DIR, config.enrollment.max_templates as usize, cipher)?;

    if !db.has_enrolled_faces(&username)? {
        anyhow::bail!("user '{username}' has no enrolled faces in {USERS_DIR}");
    }

    let mut bench = Bench {
        detector,
        recognizer,
        db,
        username,
        threshold,
    };

    let (me_correct, me_total) = bench.run_dir("YOU (should match)", &dir_me, true)?;
    let (notme_correct, notme_total) =
        bench.run_dir("OTHERS (should reject)", &dir_notme, false)?;

    println!("\n=== Summary ===");
    println!("True accept rate (you, correctly matched):  {me_correct}/{me_total}");
    println!("True reject rate (others, correctly rejected): {notme_correct}/{notme_total}");

    Ok(())
}
