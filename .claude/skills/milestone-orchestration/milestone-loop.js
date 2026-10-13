export const meta = {
  name: 'milestone-loop',
  description: 'Build a sequence of milestones: architect, implementer (local gates + push + every CI workflow green), adversarial reviewer, bounded fix round, CI verification, then a completeness critic',
  whenToUse: 'Multi-milestone builds in agentfs; args {base, goals, milestones, repo?}',
  phases: [
    { title: 'Plan', detail: 'architect (read-only) plans each milestone' },
    { title: 'Implement', detail: 'implementer builds, passes local gates, pushes, gets CI green' },
    { title: 'Review', detail: 'adversarial review of the milestone' },
    { title: 'Fix', detail: 'one fix round for blocker/major findings, CI green again' },
    { title: 'Critic', detail: 'completeness check against design §18/§19 and this run\'s goals' },
  ],
}

const REPO = args.repo || '/home/claude/Code/agents/agentfs'

// Conventions live in the repo's AGENTS.md (read by every agent); this block
// only adds what a workflow agent needs on top of it.
const CONTEXT = `
You are working on agentfs at ${REPO} (GitHub: mcurcio/agentfs, default branch main).
- Read ${REPO}/AGENTS.md first and follow it: local gates, what only CI can test, CI workflow durations, commit and push rules, docs-travel-with-code, the no-root toolchain. Then read docs/design.md (authoritative; decisions in §19) and the docs and code the brief touches.
- Do NOT install system packages globally or modify ~/.local/bin, ~/.devenv or ~/.cargo/config.toml unless AGENTS.md says how.
- After local gates pass: \`git pull --ff-only origin main\`, push to origin main (retry up to 4 times on network errors; never force-push), then wait for EVERY workflow run for YOUR pushed SHA, including the slow ones listed in AGENTS.md, and fix until all are green (at most 6 attempts). A run cancelled because your newer commit superseded it is fine; judge the runs for your final head.
- Be honest in your report: list anything unfinished, untested, or only tested locally.
`

const PLAN_SCHEMA = {
  type: 'object',
  properties: {
    summary: { type: 'string' },
    layer_decisions: { type: 'string' },
    steps: { type: 'array', items: { type: 'string' } },
    files: { type: 'array', items: { type: 'string' } },
    public_api: { type: 'string' },
    tests: { type: 'array', items: { type: 'string' } },
    ci_changes: { type: 'array', items: { type: 'string' } },
    docs_changes: { type: 'array', items: { type: 'string' } },
    dependencies: { type: 'array', items: { type: 'string' } },
    design_decisions: { type: 'array', items: { type: 'string' } },
    risks: { type: 'array', items: { type: 'string' } },
  },
  required: ['summary', 'layer_decisions', 'steps', 'files', 'public_api', 'tests', 'ci_changes', 'docs_changes', 'dependencies', 'design_decisions', 'risks'],
}

const IMPL_SCHEMA = {
  type: 'object',
  properties: {
    commits: { type: 'array', items: { type: 'string' } },
    head: { type: 'string', description: 'full SHA of HEAD (pushed to origin/main)' },
    local_gates_passed: { type: 'boolean' },
    pushed: { type: 'boolean' },
    ci_green: { type: 'boolean', description: 'every workflow run for head completed successfully' },
    ci_run_url: { type: 'string' },
    test_count: { type: 'integer' },
    summary: { type: 'string' },
    deviations: { type: 'array', items: { type: 'string' } },
    problems: { type: 'array', items: { type: 'string' } },
  },
  required: ['commits', 'head', 'local_gates_passed', 'pushed', 'ci_green', 'ci_run_url', 'test_count', 'summary', 'deviations', 'problems'],
}

const REVIEW_SCHEMA = {
  type: 'object',
  properties: {
    verdict: { type: 'string', enum: ['approve', 'changes'] },
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          severity: { type: 'string', enum: ['blocker', 'major', 'minor'] },
          file: { type: 'string' },
          summary: { type: 'string' },
          failure_scenario: { type: 'string' },
          fix: { type: 'string' },
        },
        required: ['severity', 'file', 'summary', 'failure_scenario', 'fix'],
      },
    },
  },
  required: ['verdict', 'findings'],
}

const CRITIC_SCHEMA = {
  type: 'object',
  properties: {
    acceptance_tests: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          test: { type: 'string' },
          status: { type: 'string', enum: ['covered_in_ci', 'covered_locally_only', 'partial', 'missing'] },
          evidence: { type: 'string' },
        },
        required: ['test', 'status', 'evidence'],
      },
    },
    run_goals: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          goal: { type: 'string' },
          status: { type: 'string', enum: ['done', 'partial', 'missing'] },
          evidence: { type: 'string' },
        },
        required: ['goal', 'status', 'evidence'],
      },
      description: 'each previous-review gap and each new request in this run',
    },
    gaps: { type: 'array', items: { type: 'string' } },
    open_questions: { type: 'array', items: { type: 'string' } },
    doc_inaccuracies: { type: 'array', items: { type: 'string' } },
  },
  required: ['acceptance_tests', 'run_goals', 'gaps', 'open_questions', 'doc_inaccuracies'],
}

let base = args.base
const results = []

for (const m of args.milestones) {
  log(`${m.id}: ${m.title} (base ${base.slice(0, 7)})`)

  const plan = await agent(
    `${CONTEXT}\nYou are the ARCHITECT for ${m.id}: ${m.title}. Do not edit files.\n\nBrief:\n${m.brief}\n\nRead the design sections and current code (git log, relevant modules, existing CI) and produce a concrete, implementable plan, including the CI jobs and docs it needs. Where the design leaves a question open, decide it and say so. Prefer pure-Rust dependencies; anything needing system libraries must build in CI and be feature-gated if it cannot build locally. Cover every item in the brief; if one item is genuinely infeasible, say why under risks and plan the closest honest alternative (documented as such).`,
    { label: `architect:${m.id}`, phase: 'Plan', schema: PLAN_SCHEMA, agentType: 'Plan', effort: 'high' },
  )
  if (!plan) { log(`${m.id}: no plan, stopping`); break }

  const impl = await agent(
    `${CONTEXT}\nYou are the IMPLEMENTER for ${m.id}: ${m.title}. HEAD should be ${base}.\n\nBrief:\n${m.brief}\n\nArchitect's plan (follow it; if part of it is wrong, do the right thing and record the deviation):\n${JSON.stringify(plan, null, 2)}\n\nImplement it with thorough tests, CI jobs and docs. Pass local gates, commit in coherent commits, push, and get CI green for your final head.`,
    { label: `implement:${m.id}`, phase: 'Implement', schema: IMPL_SCHEMA },
  )
  if (!impl || !impl.head) { log(`${m.id}: implementer failed, stopping`); results.push({ id: m.id, title: m.title, plan_summary: plan.summary, impl }); break }

  const review = await agent(
    `${CONTEXT}\nYou are an ADVERSARIAL REVIEWER for ${m.id}: ${m.title}. Do not edit files or push. Find what is wrong; do not praise.\n\nBrief:\n${m.brief}\n\nThe change is \`git -C ${REPO} diff ${base}..${impl.head}\`. Plan:\n${JSON.stringify(plan, null, 2)}\n\nThe implementer reported CI ${impl.ci_green ? 'green' : 'NOT green'} (${impl.ci_run_url}); verify every workflow for that SHA with gh. Check: every brief item is actually done; correctness against docs/design.md; durability and crash safety; concurrency; error handling; security of anything privileged (FUSE, containers, credentials, network transport auth, Kubernetes RBAC, Helm defaults); that tests and CI jobs actually exercise the behaviour (not tautologies, not silently skipped); and that docs match the code. Run the local gates yourself. Every finding needs a concrete failure scenario. Verdict "changes" only for at least one blocker or major.`,
    { label: `review:${m.id}`, phase: 'Review', schema: REVIEW_SCHEMA, effort: 'high' },
  )

  let final = impl
  const serious = (review?.findings || []).filter(f => f.severity !== 'minor')
  if (review && review.verdict === 'changes' && serious.length) {
    log(`${m.id}: ${serious.length} blocker/major finding(s), fixing`)
    const fix = await agent(
      `${CONTEXT}\nYou are the IMPLEMENTER fixing review findings for ${m.id}: ${m.title}. HEAD is ${impl.head}.\n\nFix every blocker and major finding (with a test that would have caught it where possible). Fix minor findings when cheap; list the rest in problems. Pass local gates, commit, push, and get CI green.\n\nFindings:\n${JSON.stringify(review.findings, null, 2)}`,
      { label: `fix:${m.id}`, phase: 'Fix', schema: IMPL_SCHEMA },
    )
    if (fix && fix.head) final = { ...fix, commits: [...impl.commits, ...fix.commits], deviations: [...impl.deviations, ...fix.deviations] }
  } else if (!impl.ci_green) {
    log(`${m.id}: review passed but CI not green, fixing CI`)
    const fix = await agent(
      `${CONTEXT}\nCI is not green for ${m.id}: ${m.title} at ${impl.head} (${impl.ci_run_url}). Find out why, fix it, and get CI green. Report as for an implementation.`,
      { label: `fix-ci:${m.id}`, phase: 'Fix', schema: IMPL_SCHEMA },
    )
    if (fix && fix.head) final = { ...fix, commits: [...impl.commits, ...fix.commits], deviations: [...impl.deviations, ...fix.deviations] }
  }

  if (final.head && final.local_gates_passed && !final.ci_green) {
    log(`${m.id}: CI not confirmed green at ${final.head.slice(0, 7)}; verifying`)
    const check = await agent(
      `${CONTEXT}\nThe ${m.id} (${m.title}) implementer stopped at ${final.head} without confirming every workflow run green (some may still have been running, e.g. bench takes ~25 min). Wait for every workflow run for the current origin/main head to finish (\`gh run watch\`). If all are green, report ci_green true with no new commits (commits: [], head: the SHA). If any failed, find out why, fix it (a flaky job must be made reliable, not just rerun), push, and get every run green. Report as for an implementation.`,
      { label: `verify-ci:${m.id}`, phase: 'Fix', schema: IMPL_SCHEMA },
    )
    if (check && check.head) final = { ...check, commits: [...final.commits, ...check.commits], deviations: [...final.deviations, ...check.deviations], problems: [...final.problems, ...check.problems] }
  }

  results.push({
    id: m.id, title: m.title, base, head: final.head, commits: final.commits,
    local_gates_passed: final.local_gates_passed, ci_green: final.ci_green, ci_run_url: final.ci_run_url,
    test_count: final.test_count, summary: final.summary, design_decisions: plan.design_decisions,
    deviations: final.deviations, problems: final.problems,
    review_verdict: review ? review.verdict : 'no review',
    review_findings: (review?.findings || []).map(f => `[${f.severity}] ${f.summary}`),
  })
  base = final.head
  if (!final.local_gates_passed || !final.ci_green) {
    log(`${m.id}: gates or CI not green at ${final.head.slice(0, 7)}; stopping before the next milestone`)
    break
  }
}

phase('Critic')
const critic = await agent(
  `${CONTEXT}\nYou are the COMPLETENESS CRITIC. Do not edit files or push. HEAD is ${base}.\n\nThis run's goals (every one must be judged in run_goals):\n${args.goals.map((g, i) => `${i + 1}. ${g}`).join('\n')}\n\nAlso: for every acceptance test in design §18 say whether it is covered in CI, covered only locally, partial or missing (cite tests, CI jobs, scripts); list what is still missing (most important first); list §19 open questions still open; list places where README/docs disagree with the code. Check every workflow run for HEAD with gh.`,
  { label: 'critic', phase: 'Critic', schema: CRITIC_SCHEMA, effort: 'high' },
)

return { milestones: results, critic }
