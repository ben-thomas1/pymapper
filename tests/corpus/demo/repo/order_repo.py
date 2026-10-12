from models.order import Order  # closes the cycle with models.order

_counter = 0


def next_order_id():
    global _counter
    _counter += 1
    return _counter


def save_order(order: Order):
    order.validate()
    return order.to_dict()
