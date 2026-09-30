def check_value(x, flag):
    if x > 0 and flag:
        return "positive and flagged"
    elif x > 0 or not flag:
        return "positive or not flagged"
    elif x == 0 and not flag:
        return "zero"
    else:
        return "negative"
