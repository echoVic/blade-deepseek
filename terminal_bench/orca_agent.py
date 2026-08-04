"""
Harbor adapter for running Orca (blade-deepseek) on Terminal-Bench.

Usage:
    harbor run -d "terminal-bench/terminal-bench-2" \
        --agent "terminal_bench.orca_agent:OrcaInstalledAgent" \
        -k 5
"""

import json
import os
import shlex
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

ORCA_VERSION = "0.3.3"
ORCA_RELEASE_URL = (
    f"https://github.com/echoVic/orca-agent/releases/download/v{ORCA_VERSION}"
    f"/orca-x86_64-unknown-linux-gnu.tar.gz"
)

ORCA_LOCAL_MUSL_BIN = str(
    Path(__file__).resolve().parent.parent
    / "target/x86_64-unknown-linux-musl/release/orca"
)


def _load_api_key() -> str:
    """Read DEEPSEEK_API_KEY from ~/.orca/auth.json, fall back to env."""
    auth_file = Path.home() / ".orca" / "auth.json"
    if auth_file.exists():
        data = json.loads(auth_file.read_text())
        if key := data.get("DEEPSEEK_API_KEY"):
            return key
    return os.environ.get("ORCA_API_KEY", "")


class OrcaInstalledAgent(BaseInstalledAgent):
    """Orca coding agent adapter for Harbor / Terminal-Bench."""

    @staticmethod
    def name() -> str:
        return "orca"

    def version(self) -> str | None:
        return ORCA_VERSION

    async def install(self, environment: BaseEnvironment) -> None:
        await self.exec_as_root(
            environment,
            command=(
                "apt-get update && apt-get install -y git ripgrep"
                " && cp /mnt/orca-bin/orca /usr/local/bin/orca"
                " && chmod +x /usr/local/bin/orca"
            ),
        )

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        env = {
            "DEEPSEEK_API_KEY": _load_api_key(),
            "ORCA_BASE_URL": os.environ.get("ORCA_BASE_URL", "https://api.deepseek.com"),
            "ORCA_MODEL": os.environ.get("ORCA_MODEL", "deepseek-v4-flash"),
        }

        cmd = (
            f"orca exec"
            f" --mode full-auto"
            f" --output-format jsonl"
            f" {shlex.quote(instruction)}"
        )

        await self.exec_as_agent(environment, command=cmd, env=env)

    def populate_context_post_run(self, context: AgentContext) -> None:
        pass
