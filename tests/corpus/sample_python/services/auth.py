def require_auth(func):
    def wrapper(*args, **kwargs):
        token = args[0] if args else None
        if token and validate_token(token):
            return func(*args, **kwargs)
        else:
            raise PermissionError("Unauthorized")
    return wrapper


def make_validator(low, high):
    def check(value):
        if value < low:
            return False
        if value > high:
            return False
        return True
    return check


def validate_token(token):
    if token.startswith("Bearer "):
        payload = token[7:]
        if len(payload) > 10:
            return True
    return False


def classify_token(token):
    classify = lambda t: "admin" if "admin" in t else "user"  # noqa: E731
    return classify(token)


def create_authenticator(secret, algorithm, expiry):
    """Outer function — true McC=1, true FO=1 (just returns inner).
    But inner 'authenticate' has 6+ calls and 3+ branches."""
    def authenticate(request):
        token = request.get("token", "")
        if not token:
            raise ValueError("Missing token")
        decoded = decode_token(token, secret, algorithm)
        if decoded.get("exp", 0) < expiry:
            raise ValueError("Expired")
        user_id = decoded.get("user_id")
        if not user_id:
            raise ValueError("Invalid token")
        role = decoded.get("role", "user")
        if role not in ("admin", "user", "service"):
            raise ValueError("Unknown role")
        return {"user_id": user_id, "role": role}
    return authenticate
