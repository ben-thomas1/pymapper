from utils.validation import validate_email, validate_length, validate_not_empty
from utils.formatting import format_name, format_currency
from services.auth import validate_token
from services.user_service import create_user
from utils.decorators import log_calls
from models.user import User


@log_calls
def run_pipeline(name, email, token):
    validate_not_empty(name)
    validate_not_empty(email)
    validate_email(email)
    validate_length(name, 1, 100)
    validate_token(token)
    formatted = format_name(name)
    user = create_user(formatted, email)
    return user


def format_receipt(user, amount):
    name = format_name(user.name)
    price = format_currency(amount)
    return f"{name}: {price}"
