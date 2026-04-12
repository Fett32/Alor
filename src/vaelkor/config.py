"""
Vaelkor configuration management.

Config lives in ~/.config/vaelkor/
Data lives in ~/.local/share/vaelkor/
"""

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
import yaml


CONFIG_DIR = Path.home() / ".config/vaelkor"
DATA_DIR = Path.home() / ".local/share/vaelkor"
SOCKET_DIR = Path("/tmp/vaelkor")

CURRENT_SCHEMA_VERSION = 1


@dataclass
class AgentConfig:
    name: str
    identity: str  # Symbol: *, ?, >, @
    role: str  # orchestrator, reviewer, etc.
    command: list[str]
    autoconnect: bool = False
    constraints: list[str] = field(default_factory=list)

    def to_dict(self) -> dict:
        return {
            "identity": self.identity,
            "role": self.role,
            "command": self.command,
            "autoconnect": self.autoconnect,
            "constraints": self.constraints,
        }

    @classmethod
    def from_dict(cls, name: str, d: dict) -> "AgentConfig":
        return cls(
            name=name,
            identity=d.get("identity", "?"),
            role=d.get("role", "agent"),
            command=d.get("command", []),
            autoconnect=d.get("autoconnect", False),
            constraints=d.get("constraints", []),
        )


@dataclass
class VaelkorConfig:
    schema_version: int = CURRENT_SCHEMA_VERSION
    default_orchestrator: str = "claude"
    auto_routing: bool = False  # V1: always false
    theme: str = "beskar"
    primary_view: str = "task_list"
    task_assignment_timeout: int = 30
    heartbeat_interval: int = 60
    agents: dict[str, AgentConfig] = field(default_factory=dict)

    def to_dict(self) -> dict:
        return {
            "schema_version": self.schema_version,
            "default_orchestrator": self.default_orchestrator,
            "auto_routing": self.auto_routing,
            "ui": {
                "theme": self.theme,
                "primary_view": self.primary_view,
            },
            "timeouts": {
                "task_assignment": self.task_assignment_timeout,
                "heartbeat": self.heartbeat_interval,
            },
        }

    @classmethod
    def from_dict(cls, d: dict) -> "VaelkorConfig":
        ui = d.get("ui", {})
        timeouts = d.get("timeouts", {})
        return cls(
            schema_version=d.get("schema_version", CURRENT_SCHEMA_VERSION),
            default_orchestrator=d.get("default_orchestrator", "claude"),
            auto_routing=d.get("auto_routing", False),
            theme=ui.get("theme", "beskar"),
            primary_view=ui.get("primary_view", "task_list"),
            task_assignment_timeout=timeouts.get("task_assignment", 30),
            heartbeat_interval=timeouts.get("heartbeat", 60),
        )


def ensure_config_dirs():
    """Create config directories if they don't exist."""
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    (CONFIG_DIR / "agents").mkdir(exist_ok=True)
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    (DATA_DIR / "sessions").mkdir(exist_ok=True)
    SOCKET_DIR.mkdir(parents=True, exist_ok=True)


def load_config() -> VaelkorConfig:
    """Load main config file."""
    ensure_config_dirs()
    config_path = CONFIG_DIR / "vaelkor.yaml"

    if config_path.exists():
        with open(config_path) as f:
            data = yaml.safe_load(f) or {}
        config = VaelkorConfig.from_dict(data)
    else:
        config = VaelkorConfig()
        save_config(config)

    # Load agent configs
    config.agents = load_agent_configs()

    return config


def save_config(config: VaelkorConfig):
    """Save main config file."""
    ensure_config_dirs()
    config_path = CONFIG_DIR / "vaelkor.yaml"

    with open(config_path, "w") as f:
        yaml.dump(config.to_dict(), f, default_flow_style=False)


def load_agent_configs() -> dict[str, AgentConfig]:
    """Load all agent config files."""
    agents = {}
    agents_dir = CONFIG_DIR / "agents"
    agents_dir.mkdir(parents=True, exist_ok=True)

    # Create defaults if no agents exist
    if not any(agents_dir.glob("*.yaml")):
        _create_default_agents()

    for agent_file in agents_dir.glob("*.yaml"):
        name = agent_file.stem
        with open(agent_file) as f:
            data = yaml.safe_load(f) or {}
        agents[name] = AgentConfig.from_dict(name, data)

    return agents


def save_agent_config(agent: AgentConfig):
    """Save an agent config file."""
    ensure_config_dirs()
    agent_path = CONFIG_DIR / "agents" / f"{agent.name}.yaml"

    with open(agent_path, "w") as f:
        yaml.dump(agent.to_dict(), f, default_flow_style=False)


def _create_default_agents():
    """Create default agent configurations."""
    ensure_config_dirs()

    claude = AgentConfig(
        name="claude",
        identity="*",
        role="orchestrator",
        command=["claude"],
        autoconnect=True,
        constraints=[],
    )
    save_agent_config(claude)

    codex = AgentConfig(
        name="codex",
        identity="?",
        role="reviewer",
        command=["codex"],
        autoconnect=False,
        constraints=["no_file_edits", "review_only"],
    )
    save_agent_config(codex)
