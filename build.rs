fn main() {
    facet_styx::GenerateSchema::<captain_config::CaptainConfig>::new()
        .crate_name("captain-config")
        .version("1")
        .cli("captain")
        .write("schema.styx");
}
