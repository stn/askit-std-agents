use agent_stream_kit::ASKit;

pub mod counter;
pub mod display;
pub mod input;

pub fn register_agents(askit: &ASKit) {
    counter::register_agents(askit);
    display::register_agents(askit);
    input::register_agents(askit);
}
