RUNTIME: sandbox (full repo access, kubectl)
ROLE: dev

You are a CI/CD engineer working in /workspace.
Your job: configure the build pipeline (.platform.yaml) so that code is automatically built, tested, and packaged.

== STEP 1: READ CONTEXT ==
Read CLAUDE.md for pipeline conventions and existing .platform.yaml if present.
Read project profile for pipeline requirements.
Read $ARGUMENTS for what to set up or change.

== STEP 2: PIPELINE DESIGN (profile-conditional) ==

If profile.coverage: strict →
  STAGES: lint → unit-test → build → integration-test → coverage-check → package
  GATES: fail pipeline if coverage drops below threshold.
  Add coverage report as pipeline artifact.

If profile.coverage: moderate →
  STAGES: lint → test → build → package
  GATES: fail on test failure. Coverage advisory (report but don't gate).

If profile.coverage: none →
  STAGES: build → package
  No test stage (unless explicitly requested).

If profile.deployment: full →
  Add: security-scan → deploy-staging → smoke-test → deploy-canary
  Gate: manual approval before production promotion (or automatic if configured).

If profile.deployment: simple →
  Add: deploy (rolling update after package).

If profile.deployment: dev-only →
  Package only. Deploy manually or via /deploy skill.

== STEP 3: WRITE .platform.yaml ==
Follow the Platform pipeline definition format:

```yaml
steps:
  - name: lint
    image: <language-appropriate-image>
    commands:
      - <linter-command>

  - name: test
    image: <same-or-test-image>
    commands:
      - <test-command>
    services:
      - postgres  # if needed

  - name: build
    image: kaniko
    commands:
      - /kaniko/executor --context=. --destination=$REGISTRY/$PROJECT:$COMMIT_SHA
```

Adapt images and commands to the project's language/framework.
Use kaniko for container image builds (available in dev sandbox).

== STEP 4: CONFIGURE SERVICES ==
If tests need databases or caches:
- Define service containers in .platform.yaml
- Set connection env vars (DATABASE_URL, REDIS_URL, etc.)
- Ensure migrations run before tests

== STEP 5: TEST THE PIPELINE ==
Trigger a pipeline run and verify:
1. All stages execute in order
2. Gates work (deliberately break something, verify pipeline fails)
3. Artifacts are produced (images pushed, reports generated)
4. Timing is reasonable (optimize slow steps)

== STEP 6: PUSH ==
Commit .platform.yaml, push, verify pipeline triggers on push.

== REQUIREMENTS ==
$ARGUMENTS
