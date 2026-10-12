def invoice_lines(order):
    """PM501: copy-pasted from services/pricing.py:receipt_lines."""
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
