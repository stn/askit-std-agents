use std::vec;

use agent_stream_kit::{
    ASKit, Agent, AgentConfig, AgentConfigEntry, AgentContext, AgentData, AgentDefinition,
    AgentError, AgentOutput, AgentStatus, AgentValue, AsAgent, AsAgentData, new_boxed,
};

static CATEGORY: &str = "Core/Input";

static CONFIG_UNIT: &str = "unit";
static CONFIG_BOOLEAN: &str = "boolean";
static CONFIG_INTEGER: &str = "integer";
static CONFIG_NUMBER: &str = "number";
static CONFIG_STRING: &str = "string";
static CONFIG_TEXT: &str = "text";
static CONFIG_OBJECT: &str = "object";

/// Unit Input
struct UnitInputAgent {
    data: AsAgentData,
}

impl AsAgent for UnitInputAgent {
    fn new(
        askit: ASKit,
        id: String,
        def_name: String,
        config: Option<AgentConfig>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            data: AsAgentData::new(askit, id, def_name, config),
        })
    }

    fn data(&self) -> &AsAgentData {
        &self.data
    }

    fn mut_data(&mut self) -> &mut AsAgentData {
        &mut self.data
    }

    fn set_config(&mut self, _config: AgentConfig) -> Result<(), AgentError> {
        // Since set_config is called even when the agent is not running,
        // we need to check the status before outputting the value.
        if *self.status() == AgentStatus::Start {
            self.try_output(AgentContext::new(), CONFIG_UNIT, AgentData::new_unit())?;
        }

        Ok(())
    }
}

pub fn register_agents(askit: &ASKit) {
    // Counter Agent
    askit.register_agent(
        AgentDefinition::new("std", "$unit_input", Some(new_boxed::<UnitInputAgent>))
            .with_title("Unit Input")
            .with_category(CATEGORY)
            .with_outputs(vec![CONFIG_UNIT])
            .with_default_config(vec![(
                CONFIG_UNIT.into(),
                AgentConfigEntry::new(AgentValue::new_unit(), "unit"),
            )]),
    );
}
