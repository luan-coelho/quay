use std::{collections::HashMap, path::PathBuf};

fn main() {
    // Register the `@lucide` library so `ui/main.slint` can
    // `import { IconDisplay, IconSet } from "@lucide";` and resolve
    // to the path provided by the `lucide-slint` build helper.
    let library = HashMap::from([(
        "lucide".to_string(),
        PathBuf::from(lucide_slint::lib()),
    )]);
    let config = slint_build::CompilerConfiguration::new().with_library_paths(library);
    slint_build::compile_with_config("ui/main.slint", config).expect("Slint build failed");
}
