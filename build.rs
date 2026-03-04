fn main() {
    facet_styx::GenerateSchema::<capn_config::CapnConfig>::new()
        .crate_name("capn-config")
        .version("1")
        .cli("capn")
        .write("schema.styx");
}
