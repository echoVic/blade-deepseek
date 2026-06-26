export const meta = {
  name: "stress-evidence",
  description: "Deterministic workflow evidence stress fixture",
  phases: ["fanout"]
};

const requestedCount = Number(args?.agent_count ?? 16);
const agentCount = Number.isFinite(requestedCount)
  ? Math.max(16, Math.min(64, Math.floor(requestedCount)))
  : 16;

const prompts = Array.from({ length: agentCount }, (_, index) => ({
  prompt: `stress evidence agent ${index + 1}`,
  team: index % 2 === 0 ? "alpha" : "beta"
}));

const results = await phase("fanout", async () =>
  parallel(
    prompts.map((item) =>
      agent(item.prompt, {
        team: item.team
      })
    )
  )
);

export default {
  status: "completed",
  agent_count: results.length,
  prompts: results.map((result) => result.prompt)
};
