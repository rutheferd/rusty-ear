use opencv::prelude::*;
use opencv::videoio::{VideoCapture, CAP_ANY};
use opencv::core::Mat;
use std::{thread, time};

pub fn capture_camera_loop() -> Result<(), Box<dyn std::error::Error>> {
    // Set up the camera capture
    let mut cam = VideoCapture::new(0, CAP_ANY)?;

    // Check if the camera is opened successfully
    if !VideoCapture::is_opened(&cam)? {
        return Err("Unable to open the camera!".into());
    }

    let mut frame = Mat::default();

    loop {
        // Capture a frame
        cam.read(&mut frame)?;

        // Ensure frame is not empty
        if frame.empty() {
            println!("Warning: Captured an empty frame.");
        } else {
            // Handle the captured frame here (e.g., stream it to WebRTC)
            println!("Captured a frame!");
        }

        // Sleep for a short duration to simulate frame rate
        thread::sleep(time::Duration::from_millis(33)); // ~30 FPS
    }
}
