from services.auth import require_auth
from services.user_service import create_user
from middleware.pipeline import run_pipeline
from utils.decorators import log_calls
from cycles import ping
from models.order import Order
from repo.order_repo import save_order
from services.pricing import calculate_price, receipt_lines
from services.invoice import invoice_lines


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
    order = Order({"vip": True}, [{"name": "mug", "qty": 2, "price": 9.5}])
    save_order(order)
    print(calculate_price(order, {"vip": True}, "SAVE5", "EU", "EUR", 0.2, 4.0, False))
    print("\n".join(receipt_lines(order) + invoice_lines(order)))


def dead_endpoint():
    pass


if __name__ == "__main__":
    main()
