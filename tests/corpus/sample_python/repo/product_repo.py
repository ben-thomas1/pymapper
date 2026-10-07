def save_product(product):
    product.validate()
    product.is_valid()      # inherited from BaseModel — will be flagged dead
    data = product.to_dict()
    text = product.serialize()  # inherited from BaseModel — will be flagged dead
    return data
