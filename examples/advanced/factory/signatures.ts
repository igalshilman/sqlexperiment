/**
 * The agents' signatures: they all write as one user, in Linear and on
 * GitHub, so what each writes opens with its name, and its part
 * (`**🛠️ Coder · parser**`). The factory tells their comments from people's
 * by it: the agents' come back as webhooks too, and are not for them.
 */

/** The agents that write, by the name they sign with ("🧭 Triage"). */
const writers = new Set<string>();

/**
 * The signature of the agent `who`: `sign(key, text)` is `text`, opening with
 * its name, and its part, if its task `key` has one (`ENG-1.parser`).
 */
export function signer(who: string) {
  writers.add(who);
  return (key: string, text: string) => {
    const part = key.split('.')[1];
    return `**${who}${part ? ` · ${part}` : ''}**\n\n${text}`;
  };
}

/** Whether `text`, a comment's body, is an agent's: it opens with a signature. */
export const byAnAgent = (text: string) => {
  const opening = text.trimStart().replace(/^\*\*/, '');
  return [...writers].some((who) => opening.startsWith(who));
};
