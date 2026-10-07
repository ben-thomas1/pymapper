from services.auth import require_auth
from services.user_service import create_user
from middleware.pipeline import run_pipeline
from utils.decorators import log_calls
from cycles import ping


@app.route("/users")  # noqa: F821
def list_users():
    users = create_user("admin", "admin@test.com")
    return users


@app.route("/health")  # noqa: F821
def health_check():
    return "ok"


@log_calls
def main():
    require_auth(lambda: None)
    create_user("alice", "alice@example.com")
    run_pipeline("bob", "bob@example.com", "Bearer token123")
    ping()


def dead_endpoint():
    pass
