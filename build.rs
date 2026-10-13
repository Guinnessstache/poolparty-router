// Gives the Windows .exe its icon and file details. Skipped elsewhere, and a
// missing resource compiler only warns, so the build never fails over an icon.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico")
        .set("ProductName", "PoolParty Router")
        .set("FileDescription", "PoolParty Router")
        .set("CompanyName", "GuinnessStache");
    if let Err(e) = res.compile() {
        println!("cargo:warning=no icon embedded: {e}");
    }
}
