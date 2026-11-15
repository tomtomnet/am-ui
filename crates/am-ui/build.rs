//! Packs the application's own icons into the program (GResource).

fn main() {
    glib_build_tools::compile_resources(&["resources"], "resources/resources.gresource.xml", "am-ui.gresource");
}
