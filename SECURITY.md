# Security

Report vulnerabilities privately through [GitHub's security advisory form](https://github.com/anur4ag/ballast/security/advisories/new).
Do not include credentials or private agent transcripts in reports.
If private reporting is unavailable, open an issue asking for a private contact without disclosing exploit details.

Ballast runs as your user and controls only processes it can attribute to agent work.
It is a resource guardian, not a security sandbox for hostile agents.
Hooks fail open if the daemon is unavailable.
Only the latest release receives fixes during the alpha period.

Release archives include SHA-256 checksums and the APT repository has a dedicated signing key.
Checksums detect damaged downloads; they do not independently authenticate a compromised release account.
