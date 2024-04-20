use cfg_aliases::cfg_aliases;

fn main() {
    // Setup cfg aliases
    cfg_aliases! {
        native : { all(feature = "native", not(feature = "web"), not(target_arch = "wasm32")) },
        web : { all(feature = "web", target_arch = "wasm32", not(feature = "native")) },
        not_supported: { not(any(native, web)) },
    }
}
