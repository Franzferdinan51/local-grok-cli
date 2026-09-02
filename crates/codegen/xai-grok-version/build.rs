fn main() {
    println!("cargo:rerun-if-env-changed=GROK_VERSION");
    println!("cargo:rerun-if-env-changed=GROK_LOCAL_VERSION");
    println!("cargo:rerun-if-changed=../../../SOURCE_REV");
}
