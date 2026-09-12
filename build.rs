//! Embeds the application icon in the Windows executable, so Explorer and the
//! taskbar have one before the window ever opens. A no-op everywhere else.

fn main() {
    println!("cargo:rerun-if-changed=assets/remail.ico");

    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/remail.ico");
        if let Err(e) = resource.compile() {
            // A missing resource compiler should not stop the build; the
            // application still runs, just without an icon on the binary.
            println!("cargo:warning=could not embed the icon: {e}");
        }
    }
}
