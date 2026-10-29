fn main() {
    let out = std::process::Command::new("diskutil").args(["info","-plist","disk0"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    match dmdisk::plist::parse(&text) {
        Ok(v) => {
            for key in ["DeviceIdentifier","VirtualOrPhysical","TotalSize","Internal","MediaName","DeviceNode"] {
                println!("{key} = {:?}", v.get(key).map(|x| format!("{x:?}")));
            }
        }
        Err(e) => println!("parse error: {e}"),
    }
}
