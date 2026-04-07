use std::process::Command;

fn main() {
    let output = Command::new("cargo")
        .arg("check")
        .arg("--message-format=json")
        .current_dir("/Users/sw/heart-portal")
        .output()
        .expect("Failed to run cargo check");

    println!("Exit status: {}", output.status);
    println!("STDOUT:\n{}", String::from_utf8_lossy(&output.stdout));
    println!("STDERR:\n{}", String::from_utf8_lossy(&output.stderr));
}