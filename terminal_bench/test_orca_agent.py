import asyncio
import sys
import tempfile
import types
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch


def _install_harbor_stubs() -> None:
    modules = {
        "harbor": types.ModuleType("harbor"),
        "harbor.agents": types.ModuleType("harbor.agents"),
        "harbor.agents.base": types.ModuleType("harbor.agents.base"),
        "harbor.agents.installed": types.ModuleType("harbor.agents.installed"),
        "harbor.agents.installed.base": types.ModuleType("harbor.agents.installed.base"),
        "harbor.environments": types.ModuleType("harbor.environments"),
        "harbor.environments.base": types.ModuleType("harbor.environments.base"),
        "harbor.models": types.ModuleType("harbor.models"),
        "harbor.models.agent": types.ModuleType("harbor.models.agent"),
        "harbor.models.agent.context": types.ModuleType("harbor.models.agent.context"),
    }

    class BaseInstalledAgent:
        pass

    class BaseAgent:
        pass

    class BaseEnvironment:
        pass

    class AgentContext:
        __slots__ = (
            "n_input_tokens",
            "n_cache_tokens",
            "n_output_tokens",
            "cost_usd",
            "rollout_details",
            "metadata",
        )

    modules["harbor.agents.base"].BaseAgent = BaseAgent
    modules["harbor.agents.installed.base"].BaseInstalledAgent = BaseInstalledAgent
    modules["harbor.agents.installed.base"].with_prompt_template = lambda fn: fn
    modules["harbor.environments.base"].BaseEnvironment = BaseEnvironment
    modules["harbor.models.agent.context"].AgentContext = AgentContext
    sys.modules.update(modules)


_install_harbor_stubs()

from terminal_bench import orca_agent, orca_external


class OrcaInstalledAgentTests(unittest.TestCase):
    @patch("terminal_bench.orca_agent.subprocess.run")
    def test_version_is_derived_from_mounted_binary(self, run) -> None:
        run.return_value = SimpleNamespace(stdout="orca 0.3.4\n")

        self.assertEqual(orca_agent.OrcaInstalledAgent().version(), "0.3.4")
        run.assert_called_once_with(
            [orca_agent.ORCA_LOCAL_MUSL_BIN, "--version"],
            capture_output=True,
            check=True,
            text=True,
        )

    def test_run_persists_trajectory_without_extending_context(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = orca_agent.OrcaInstalledAgent()
            agent.logs_dir = Path(directory)
            agent.exec_as_agent = AsyncMock(
                return_value=SimpleNamespace(stdout='{"type":"turn.completed"}\n')
            )
            context = orca_agent.AgentContext()

            asyncio.run(agent.run("finish the task", SimpleNamespace(), context))

            expected = '{"type":"turn.completed"}\n'
            self.assertFalse(hasattr(context, "output"))
            self.assertEqual(
                (Path(directory) / "trajectory.jsonl").read_text(encoding="utf-8"),
                expected,
            )

    def test_external_run_does_not_extend_context(self) -> None:
        environment = SimpleNamespace(
            exec=AsyncMock(return_value=SimpleNamespace(stdout="completed\n"))
        )
        context = orca_external.AgentContext()

        asyncio.run(
            orca_external.OrcaExternalAgent().run(
                "finish the task", environment, context
            )
        )

        self.assertFalse(hasattr(context, "output"))
        environment.exec.assert_awaited_once()

    def test_readme_uses_supported_harbor_filters(self) -> None:
        readme = (Path(__file__).parent / "README.md").read_text(encoding="utf-8")

        self.assertNotIn("--filter-difficulty", readme)
        self.assertIn("--include-task-name", readme)


if __name__ == "__main__":
    unittest.main()
