fn main() {
    // Windows releases use MSVC. GNU cross-checks use napi's dynamic symbol
    // resolution and do not need the libnode import archive used for linking.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu")
    {
        napi_build::setup();
    }
}
