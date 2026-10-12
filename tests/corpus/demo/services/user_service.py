from utils.validation import validate_email
from utils.formatting import format_name
from utils.decorators import log_calls
from models.user import User
from repo.user_repo import save_user


@log_calls
def create_user(name, email):
    # Calls validate_email 5 times — FO will show 7+ but unique callees = 3
    validate_email(email)
    validate_email(name + "@check")
    validate_email(email.lower())
    validate_email(email.upper())
    validate_email(email.strip())
    formatted = format_name(name)
    user = User(formatted, email)
    save_user(user)
    return user


def get_user_display(user):
    name = format_name(user.name)
    return name
