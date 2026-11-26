# Security policy

Zarya is an experimental research prototype. It has not had an independent security audit and should not protect real data yet.

## Reporting a vulnerability

Please do not open a public issue for a vulnerability. Use GitHub's private reporting instead: open the **Security** tab of this repository and choose **Report a vulnerability**.

Useful reports include:

- the Zarya version (`zarya --version`) and your operating system,
- the smallest program, policy and input that show the problem,
- what you expected and what happened.

Examples of what counts as a vulnerability here:

- a program reads a resource it did not declare, or one the operator's policy denies;
- a value reaches the output without being covered by `zarya --label`;
- output that breaks its contract is accepted;
- the runtime crashes, hangs or exceeds its limits on some input.

You will get an answer as soon as possible. Fixes are released on the `main` branch, with credit to the reporter if they wish.
