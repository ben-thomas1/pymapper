import json  # PM405: never used

from models.order import Order


def calculate_price(order, customer, coupon, region, currency, tax_rate, shipping, express):
    """PM101/PM102/PM103/PM105: too many parameters, branches and nesting levels."""
    total = order.total()
    if customer is not None:
        if customer.get("vip"):
            if region == "EU":
                if currency == "EUR":
                    if total > 1000:
                        total *= 0.85
                    elif total > 500:
                        total *= 0.9
                    else:
                        total *= 0.95
                elif currency == "GBP":
                    total *= 0.92
                else:
                    total *= 0.97
            elif region == "US" and express:
                total *= 0.93
            else:
                total *= 0.98
        elif customer.get("new") and coupon:
            total -= 5
    if coupon and coupon.startswith("SAVE"):
        total -= int(coupon[4:] or 0)
    for item in order.items:
        if item.get("fragile") or item.get("oversize"):
            shipping += 4
    if express:
        shipping *= 2
    return round(total * (1 + tax_rate) + shipping, 2)


def receipt_lines(order):
    """PM501: duplicated in services/invoice.py."""
    lines = []
    for item in order.items:
        name = item["name"].strip().title()
        qty = int(item["qty"])
        price = float(item["price"])
        subtotal = qty * price
        lines.append(f"{name:<20} {qty:>3} x {price:>8.2f} = {subtotal:>9.2f}")
    lines.append("-" * 46)
    lines.append(f"{'TOTAL':<36} {order.total():>9.2f}")
    return lines


def legacy_discount(total):  # PM401: never called
    return total * 0.9


class PromoEngine:  # PM402: never used
    def apply(self, order: Order):
        return order.total() * 0.8
