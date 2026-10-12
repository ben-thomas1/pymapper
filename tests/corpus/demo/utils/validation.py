def validate_email(email):
    if "@" not in email:
        return False
    return True


def validate_length(value, min_len, max_len):
    if len(value) < min_len:
        return False
    if len(value) > max_len:
        return False
    return True


def validate_range(value, low, high):
    if value < low:
        return False
    if value > high:
        return False
    return True


def validate_not_empty(value):
    if not value:
        return False
    return True
