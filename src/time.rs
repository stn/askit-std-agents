use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::vec;

use async_trait::async_trait;
use chrono::{DateTime, Local, Utc};
use cron::Schedule;
use log;
use regex::Regex;
use tokio::task::JoinHandle;

use agent_stream_kit::{
    ASKit, Agent, AgentConfig, AgentConfigEntry, AgentContext, AgentData, AgentDefinition,
    AgentError, AgentOutput, AgentStatus, AgentValue, AsAgent, AsAgentData, new_boxed,
};

// Delay Agent
struct DelayAgent {
    data: AsAgentData,
    num_waiting_data: Arc<Mutex<i64>>,
}

#[async_trait]
impl AsAgent for DelayAgent {
    fn new(
        askit: ASKit,
        id: String,
        def_name: String,
        config: Option<AgentConfig>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            data: AsAgentData::new(askit, id, def_name, config),
            num_waiting_data: Arc::new(Mutex::new(0)),
        })
    }

    fn data(&self) -> &AsAgentData {
        &self.data
    }

    fn mut_data(&mut self) -> &mut AsAgentData {
        &mut self.data
    }

    async fn process(&mut self, ctx: AgentContext, data: AgentData) -> Result<(), AgentError> {
        let config = self.config().ok_or(AgentError::NoConfig)?;
        let delay_ms = config.get_integer_or(CONFIG_DELAY, DELAY_MS_DEFAULT);
        let max_num_data = config.get_integer_or(CONFIG_MAX_NUM_DATA, MAX_NUM_DATA_DEFAULT);

        // To avoid generating too many timers
        {
            let num_waiting_data = self.num_waiting_data.clone();
            let mut num_waiting_data = num_waiting_data.lock().unwrap();
            if *num_waiting_data >= max_num_data {
                return Ok(());
            }
            *num_waiting_data += 1;
        }

        tokio::time::sleep(Duration::from_millis(delay_ms as u64)).await;

        self.try_output(ctx.clone(), ctx.ch().to_string(), data.clone())?;

        let mut num_waiting_data = self.num_waiting_data.lock().unwrap();
        *num_waiting_data -= 1;

        Ok(())
    }
}

// Interval Timer Agent
struct IntervalTimerAgent {
    data: AsAgentData,
    timer_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    interval_ms: u64,
}

impl IntervalTimerAgent {
    fn start_timer(&mut self) -> Result<(), AgentError> {
        let timer_handle = self.timer_handle.clone();
        let interval_ms = self.interval_ms;

        let askit = self.askit().clone();
        let agent_id = self.id().to_string();
        let handle = self.runtime().spawn(async move {
            loop {
                // Sleep for the configured interval
                tokio::time::sleep(tokio::time::Duration::from_millis(interval_ms)).await;

                // Check if we've been stopped
                if let Ok(handle) = timer_handle.lock() {
                    if handle.is_none() {
                        break;
                    }
                }

                // Create a unit output
                if let Err(e) = askit.try_send_agent_out(
                    agent_id.clone(),
                    AgentContext::new_with_ch(CH_UNIT),
                    AgentData::new_unit(),
                ) {
                    log::error!("Failed to send interval timer output: {}", e);
                }
            }
        });

        // Store the timer handle
        if let Ok(mut timer_handle) = self.timer_handle.lock() {
            *timer_handle = Some(handle);
        }

        Ok(())
    }

    fn stop_timer(&mut self) -> Result<(), AgentError> {
        // Cancel the timer
        if let Ok(mut timer_handle) = self.timer_handle.lock() {
            if let Some(handle) = timer_handle.take() {
                handle.abort();
            }
        }
        Ok(())
    }
}

impl AsAgent for IntervalTimerAgent {
    fn new(
        askit: ASKit,
        id: String,
        def_name: String,
        config: Option<AgentConfig>,
    ) -> Result<Self, AgentError> {
        let interval = config
            .as_ref()
            .and_then(|c| c.get_string(CONFIG_INTERVAL))
            .unwrap_or_else(|| INTERVAL_DEFAULT.to_string());
        let interval_ms = parse_duration_to_ms(&interval)?;

        Ok(Self {
            data: AsAgentData::new(askit, id, def_name, config),
            timer_handle: Default::default(),
            interval_ms,
        })
    }

    fn data(&self) -> &AsAgentData {
        &self.data
    }

    fn mut_data(&mut self) -> &mut AsAgentData {
        &mut self.data
    }

    fn start(&mut self) -> Result<(), AgentError> {
        self.start_timer()
    }

    fn stop(&mut self) -> Result<(), AgentError> {
        self.stop_timer()
    }

    fn set_config(&mut self, config: AgentConfig) -> Result<(), AgentError> {
        // Check if interval has changed
        if let Some(interval) = config.get_string(CONFIG_INTERVAL) {
            let new_interval = parse_duration_to_ms(&interval)?;
            if new_interval != self.interval_ms {
                self.interval_ms = new_interval;
                if *self.status() == AgentStatus::Start {
                    // Restart the timer with the new interval
                    self.stop_timer()?;
                    self.start_timer()?;
                }
            }
        }
        Ok(())
    }
}

// OnStart
struct OnStartAgent {
    data: AsAgentData,
}

impl AsAgent for OnStartAgent {
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

    fn start(&mut self) -> Result<(), AgentError> {
        let config = self.config().ok_or(AgentError::NoConfig)?;
        let delay_ms = config.get_integer_or(CONFIG_DELAY, DELAY_MS_DEFAULT);

        let askit = self.askit().clone();
        let agent_id = self.id().to_string();

        self.runtime().spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms as u64)).await;

            if let Err(e) = askit.try_send_agent_out(
                agent_id,
                AgentContext::new_with_ch(CH_UNIT),
                AgentData::new_unit(),
            ) {
                log::error!("Failed to send delayed output: {}", e);
            }
        });

        Ok(())
    }
}

// Schedule Timer Agent
struct ScheduleTimerAgent {
    data: AsAgentData,
    cron_schedule: Option<Schedule>,
    timer_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl ScheduleTimerAgent {
    fn start_timer(&mut self) -> Result<(), AgentError> {
        let Some(schedule) = &self.cron_schedule else {
            return Err(AgentError::InvalidConfig("No schedule defined".into()));
        };

        let askit = self.askit().clone();
        let agent_id = self.id().to_string();
        let timer_handle = self.timer_handle.clone();
        let schedule = schedule.clone();

        let handle = self.runtime().spawn(async move {
            loop {
                // Calculate the next time this schedule should run
                let now: DateTime<Utc> = Utc::now();
                let next = match schedule.upcoming(Utc).next() {
                    Some(next_time) => next_time,
                    None => {
                        log::error!("No upcoming schedule times found");
                        break;
                    }
                };

                // Calculate the duration until the next scheduled time
                let duration = match (next - now).to_std() {
                    Ok(duration) => duration,
                    Err(e) => {
                        log::error!("Failed to calculate duration until next schedule: {}", e);
                        // If we can't calculate the duration, sleep for a short time and try again
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        continue;
                    }
                };

                let next_local = next.with_timezone(&Local);
                log::debug!(
                    "Scheduling timer for '{}' to fire at {} (in {:?})",
                    agent_id,
                    next_local.format("%Y-%m-%d %H:%M:%S %z"),
                    duration
                );

                // Sleep until the next scheduled time
                tokio::time::sleep(duration).await;

                // Check if we've been stopped
                if let Ok(handle) = timer_handle.lock() {
                    if handle.is_none() {
                        break;
                    }
                }

                // Get the current local timestamp (in seconds)
                let current_local_time = Local::now().timestamp();

                // Output the timestamp as an integer
                if let Err(e) = askit.try_send_agent_out(
                    agent_id.clone(),
                    AgentContext::new_with_ch(CH_TIME),
                    AgentData::new_integer(current_local_time),
                ) {
                    log::error!("Failed to send schedule timer output: {}", e);
                }
            }
        });

        // Store the timer handle
        if let Ok(mut timer_handle) = self.timer_handle.lock() {
            *timer_handle = Some(handle);
        }

        Ok(())
    }

    fn stop_timer(&mut self) -> Result<(), AgentError> {
        // Cancel the timer
        if let Ok(mut timer_handle) = self.timer_handle.lock() {
            if let Some(handle) = timer_handle.take() {
                handle.abort();
            }
        }
        Ok(())
    }

    fn parse_schedule(&mut self, schedule_str: &str) -> Result<(), AgentError> {
        if schedule_str.trim().is_empty() {
            self.cron_schedule = None;
            return Ok(());
        }

        let schedule = Schedule::from_str(schedule_str).map_err(|e| {
            AgentError::InvalidConfig(format!("Invalid cron schedule '{}': {}", schedule_str, e))
        })?;
        self.cron_schedule = Some(schedule);
        Ok(())
    }
}

impl AsAgent for ScheduleTimerAgent {
    fn new(
        askit: ASKit,
        id: String,
        def_name: String,
        config: Option<AgentConfig>,
    ) -> Result<Self, AgentError> {
        let mut agent = Self {
            data: AsAgentData::new(askit, id, def_name, config.clone()),
            cron_schedule: None,
            timer_handle: Default::default(),
        };

        if let Some(config) = config {
            if let Some(schedule_str) = config.get_string(CONFIG_SCHEDULE) {
                if !schedule_str.is_empty() {
                    agent.parse_schedule(&schedule_str)?;
                }
            }
        }

        Ok(agent)
    }

    fn data(&self) -> &AsAgentData {
        &self.data
    }

    fn mut_data(&mut self) -> &mut AsAgentData {
        &mut self.data
    }

    fn start(&mut self) -> Result<(), AgentError> {
        if self.cron_schedule.is_some() {
            self.start_timer()?;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), AgentError> {
        self.stop_timer()
    }

    fn set_config(&mut self, config: AgentConfig) -> Result<(), AgentError> {
        // Check if schedule has changed
        if let Some(schedule_str) = config.get_string(CONFIG_SCHEDULE) {
            self.parse_schedule(&schedule_str)?;

            if *self.status() == AgentStatus::Start {
                // Restart the timer with the new schedule
                self.stop_timer()?;
                if self.cron_schedule.is_some() {
                    self.start_timer()?;
                }
            }
        }
        Ok(())
    }
}

// Throttle agent
struct ThrottleTimeAgent {
    data: AsAgentData,
    timer_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    time_ms: u64,
    max_num_data: i64,
    waiting_data: Arc<Mutex<Vec<(AgentContext, AgentData)>>>,
}

impl ThrottleTimeAgent {
    fn start_timer(&mut self) -> Result<(), AgentError> {
        let timer_handle = self.timer_handle.clone();
        let time_ms = self.time_ms;

        let waiting_data = self.waiting_data.clone();
        let askit = self.askit().clone();
        let agent_id = self.id().to_string();

        let handle = self.runtime().spawn(async move {
            loop {
                // Sleep for the configured interval
                tokio::time::sleep(tokio::time::Duration::from_millis(time_ms)).await;

                // Check if we've been stopped
                let mut handle = timer_handle.lock().unwrap();
                if handle.is_none() {
                    break;
                }

                // process the waiting data
                let mut wd = waiting_data.lock().unwrap();
                if wd.len() > 0 {
                    // If there are data waiting, output the first one
                    let (ctx, data) = wd.remove(0);
                    askit
                        .try_send_agent_out(agent_id.clone(), ctx, data)
                        .unwrap_or_else(|e| {
                            log::error!("Failed to send delayed output: {}", e);
                        });
                }

                // If there are no data waiting, we stop the timer
                if wd.len() == 0 {
                    handle.take();
                    break;
                }
            }
        });

        // Store the timer handle
        if let Ok(mut timer_handle) = self.timer_handle.lock() {
            *timer_handle = Some(handle);
        }

        Ok(())
    }

    fn stop_timer(&mut self) -> Result<(), AgentError> {
        // Cancel the timer
        if let Ok(mut timer_handle) = self.timer_handle.lock() {
            if let Some(handle) = timer_handle.take() {
                handle.abort();
            }
        }
        Ok(())
    }
}

#[async_trait]
impl AsAgent for ThrottleTimeAgent {
    fn new(
        askit: ASKit,
        id: String,
        def_name: String,
        config: Option<AgentConfig>,
    ) -> Result<Self, AgentError> {
        let time = config
            .as_ref()
            .and_then(|c| c.get_string(CONFIG_TIME))
            .unwrap_or_else(|| TIME_DEFAULT.to_string());
        let time_ms = parse_duration_to_ms(&time)?;

        let max_num_data = config
            .as_ref()
            .and_then(|c| c.get_integer(CONFIG_MAX_NUM_DATA))
            .unwrap_or(0);

        Ok(Self {
            data: AsAgentData::new(askit, id, def_name, config),
            timer_handle: Default::default(),
            time_ms,
            max_num_data,
            waiting_data: Arc::new(Mutex::new(vec![])),
        })
    }

    fn data(&self) -> &AsAgentData {
        &self.data
    }

    fn mut_data(&mut self) -> &mut AsAgentData {
        &mut self.data
    }

    fn stop(&mut self) -> Result<(), AgentError> {
        self.stop_timer()
    }

    fn set_config(&mut self, config: AgentConfig) -> Result<(), AgentError> {
        // Check if interval has changed
        if let Some(time) = config.get_string(CONFIG_TIME) {
            let new_time = parse_duration_to_ms(&time)?;
            if new_time != self.time_ms {
                self.time_ms = new_time;
            }
        }
        // Check if max_num_data has changed
        if let Some(max_num_data) = config.get_integer(CONFIG_MAX_NUM_DATA) {
            if self.max_num_data != max_num_data {
                let mut wd = self.waiting_data.lock().unwrap();
                let wd_len = wd.len();
                if max_num_data >= 0 && wd_len > (max_num_data as usize) {
                    // If we have reached the max data to keep, we drop the oldest one
                    wd.drain(0..(wd_len - (max_num_data as usize)));
                }
                self.max_num_data = max_num_data;
            }
        }
        Ok(())
    }

    async fn process(&mut self, ctx: AgentContext, data: AgentData) -> Result<(), AgentError> {
        if self.timer_handle.lock().unwrap().is_some() {
            // If the timer is running, we just add the data to the waiting list
            let mut wd = self.waiting_data.lock().unwrap();

            // If max_num_data is 0, we don't need to keep any data
            if self.max_num_data == 0 {
                return Ok(());
            }

            wd.push((ctx, data));
            if self.max_num_data > 0 && wd.len() > self.max_num_data as usize {
                // If we have reached the max data to keep, we drop the oldest one
                wd.remove(0);
            }

            return Ok(());
        }

        // Start the timer
        self.start_timer()?;

        // Output the data
        let ch = ctx.ch().to_string();
        self.try_output(ctx, ch, data)?;

        Ok(())
    }
}

// Parse time duration strings like "2s", "10m", "200ms"
fn parse_duration_to_ms(duration_str: &str) -> Result<u64, AgentError> {
    const MIN_DURATION: u64 = 10;

    // Regular expression to match number followed by optional unit
    let re = Regex::new(r"^(\d+)(?:([a-zA-Z]+))?$").expect("Failed to compile regex");

    if let Some(captures) = re.captures(duration_str.trim()) {
        let value: u64 = captures.get(1).unwrap().as_str().parse().map_err(|e| {
            AgentError::InvalidConfig(format!(
                "Invalid number in duration '{}': {}",
                duration_str, e
            ))
        })?;

        // Get the unit if present, default to "s" (seconds)
        let unit = captures
            .get(2)
            .map_or("s".to_string(), |m| m.as_str().to_lowercase());

        // Convert to milliseconds based on unit
        let milliseconds = match unit.as_str() {
            "ms" => value,               // already in milliseconds
            "s" => value * 1000,         // seconds to milliseconds
            "m" => value * 60 * 1000,    // minutes to milliseconds
            "h" => value * 3600 * 1000,  // hours to milliseconds
            "d" => value * 86400 * 1000, // days to milliseconds
            _ => {
                return Err(AgentError::InvalidConfig(format!(
                    "Unknown time unit: {}",
                    unit
                )));
            }
        };

        // Ensure we don't return less than the minimum duration
        Ok(std::cmp::max(milliseconds, MIN_DURATION))
    } else {
        // If the string doesn't match the pattern, try to parse it as a plain number
        // and assume it's in seconds
        let value: u64 = duration_str.parse().map_err(|e| {
            AgentError::InvalidConfig(format!("Invalid duration format '{}': {}", duration_str, e))
        })?;
        Ok(std::cmp::max(value * 1000, MIN_DURATION)) // Convert to ms
    }
}

static AGENT_KIND: &str = "Agent";
static CATEGORY: &str = "Core/Time";

static CH_TIME: &str = "time";
static CH_UNIT: &str = "unit";

static CONFIG_DELAY: &str = "delay";
static CONFIG_MAX_NUM_DATA: &str = "max_num_data";
static CONFIG_INTERVAL: &str = "interval";
static CONFIG_SCHEDULE: &str = "schedule";
static CONFIG_TIME: &str = "time";

const DELAY_MS_DEFAULT: i64 = 1000; // 1 second in milliseconds
const MAX_NUM_DATA_DEFAULT: i64 = 10;
static INTERVAL_DEFAULT: &str = "10s";
static TIME_DEFAULT: &str = "1s";

pub fn register_agents(askit: &ASKit) {
    // Delay Agent
    askit.register_agent(
        AgentDefinition::new(AGENT_KIND, "std_delay", Some(new_boxed::<DelayAgent>))
            .with_title("Delay")
            .with_description("Delays output by a specified time")
            .with_category(CATEGORY)
            .with_inputs(vec!["*"])
            .with_outputs(vec!["*"])
            .with_default_config(vec![
                (
                    CONFIG_DELAY.into(),
                    AgentConfigEntry::new(AgentValue::new_integer(DELAY_MS_DEFAULT), "integer")
                        .with_title("delay (ms)"),
                ),
                (
                    CONFIG_MAX_NUM_DATA.into(),
                    AgentConfigEntry::new(AgentValue::new_integer(MAX_NUM_DATA_DEFAULT), "integer")
                        .with_title("max num data"),
                ),
            ]),
    );

    // Interval Timer Agent
    askit.register_agent(
        AgentDefinition::new(
            AGENT_KIND,
            "std_interval_timer",
            Some(new_boxed::<IntervalTimerAgent>),
        )
        .with_title("Interval Timer")
        .with_description("Outputs a unit signal at specified intervals")
        .with_category(CATEGORY)
        .with_outputs(vec![CH_UNIT])
        .with_default_config(vec![(
            CONFIG_INTERVAL.into(),
            AgentConfigEntry::new(AgentValue::new_string(INTERVAL_DEFAULT), "string")
                .with_description("(ex. 10s, 5m, 100ms, 1h, 1d)"),
        )]),
    );

    // OnStart
    askit.register_agent(
        AgentDefinition::new(AGENT_KIND, "std_on_start", Some(new_boxed::<OnStartAgent>))
            .with_title("On Start")
            .with_category(CATEGORY)
            .with_outputs(vec![CH_UNIT])
            .with_default_config(vec![(
                CONFIG_DELAY.into(),
                AgentConfigEntry::new(AgentValue::new_integer(DELAY_MS_DEFAULT), "integer")
                    .with_title("delay (ms)"),
            )]),
    );

    // Schedule Timer Agent
    askit.register_agent(
        AgentDefinition::new(
            AGENT_KIND,
            "std_schedule_timer",
            Some(new_boxed::<ScheduleTimerAgent>),
        )
        .with_title("Schedule Timer")
        .with_category(CATEGORY)
        .with_outputs(vec![CH_TIME])
        .with_default_config(vec![(
            CONFIG_SCHEDULE.into(),
            AgentConfigEntry::new(AgentValue::new_string("0 0 * * * *"), "string")
                .with_description("sec min hour day month week year"),
        )]),
    );

    // Throttle Time Agent
    askit.register_agent(
        AgentDefinition::new(
            AGENT_KIND,
            "std_throttle_time",
            Some(new_boxed::<ThrottleTimeAgent>),
        )
        .with_title("Throttle Time")
        .with_category(CATEGORY)
        .with_inputs(vec!["*"])
        .with_outputs(vec!["*"])
        .with_default_config(vec![
            (
                CONFIG_TIME.into(),
                AgentConfigEntry::new(AgentValue::new_string(TIME_DEFAULT), "string")
                    .with_description("(ex. 10s, 5m, 100ms, 1h, 1d)"),
            ),
            (
                CONFIG_MAX_NUM_DATA.into(),
                AgentConfigEntry::new(AgentValue::new_integer(0), "integer")
                    .with_title("max num data")
                    .with_description("0: no data, -1: all data"),
            ),
        ]),
    );
}
