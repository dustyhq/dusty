"""Build a Dusty environment private to a loopback forge scenario."""

import os


def fixture_env(work, **overrides):
    # credential.rs and detect.rs discover sign-ins, host claims and endpoints
    # from this environment. Keep only the overrides supplied by the fixture.
    prefixes = ("GH_", "GITHUB_", "GITLAB_", "GL_", "GLAB_", "GITEA_", "FORGEJO_",
                "TEA_", "FJ_", "DUSTY_FORGE_")
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(prefixes) and key not in ("OAUTH_TOKEN", "DUSTY_GIT_CACHE_DIR")}
    for name in ("CONFIG", "STATE", "DATA", "CACHE"):
        path = work / name.lower()
        path.mkdir()
        env[f"XDG_{name}_HOME"] = str(path)
    home = work / "home"
    home.mkdir()
    env.update(
        HOME=str(home),  # macOS tea/fj stores can fall back to HOME even with XDG set.
        DUSTY_CONFIG_HOME=env["XDG_CONFIG_HOME"],
        DUSTY_STATE_HOME=env["XDG_STATE_HOME"],
        GH_CONFIG_DIR=str(work / "config" / "gh"),
        GLAB_CONFIG_DIR=str(work / "config" / "glab-cli"),
    )
    env.update(overrides)
    return env
