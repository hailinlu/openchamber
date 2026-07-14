// Default prompt fragments shipped with OpenChamber. They appear as
// placeholder text in the Settings → Agents → System Prompt textarea and
// as initial seed text in Settings → Behavior → Global AGENTS.md.
// Users can overwrite them — these are hints, not hard rules.

export const DEFAULT_AGENT_PROMPT_FRAGMENT =
  'When updating the todo list with TodoWrite, NEVER remove or overwrite tasks already marked as `completed`. ' +
  'Completed tasks are a progress record and must stay visible until the entire list is reset. ' +
  'When splitting work into sub-tasks, insert new entries into the pending queue or replace only `pending`/`in_progress` items — ' +
  'never `completed` ones. ' +
  'Sub-tasks MUST use fresh `content` strings — never reuse the `content` of a `completed` task, and never reuse the `content` of another pending/in_progress item either. ' +
  'When updating a task\'s `content`, do not reword or paraphrase the original description. You may APPEND a short result suffix ' +
  '(e.g. "✅", "(68.2%)", "(done)") to a completed task to record the outcome, but the original description must remain intact at the front.';

export const DEFAULT_AGENTS_MD_FRAGMENT =
  '## TodoWrite usage\n\n' +
  'You may freely use `todowrite` to break complex work into sub-tasks, but the visible todo list must remain a faithful progress record.\n\n' +
  'Four hard rules:\n' +
  '1. **Never remove or overwrite `completed` tasks.** They are a progress log and stay visible until the whole list is reset.\n' +
  '2. **Sub-tasks replace `pending`/`in_progress` entries, not `completed` ones.** When splitting work, insert the new sub-task(s) into the pending queue, or replace only items that are still pending or in_progress.\n' +
  '3. **Sub-tasks use fresh `content` strings.** Never reuse the `content` of a `completed` task for a new split, and never reuse another pending/in_progress item\'s `content` either — every entry in the list must have a unique `content` string.\n' +
  '4. **Never reword a task\'s original `content`.** When updating a todo, keep the original description intact. You may APPEND a short result suffix to a `completed` task to record the outcome (e.g. "✅", "(68.2%)", "(done)"), but the leading description must not be reworded, paraphrased, or have its tool-call tags altered.';