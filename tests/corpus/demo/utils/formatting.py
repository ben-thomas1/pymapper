def format_name(name):
    return name.strip().title()


def format_currency(amount):
    return f"${amount:.2f}"


def format_percentage(value):
    return f"{value * 100:.1f}%"
