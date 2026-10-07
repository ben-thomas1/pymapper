def complex_function(x, y, z):
    if x > 0:
        if y > 0:
            for i in range(z):
                if i % 2 == 0:
                    print(i)
                else:
                    print(-i)
        elif y == 0:
            while x > 0:
                x -= 1
        else:
            try:
                result = x / y
            except ZeroDivisionError:
                result = 0
    else:
        if z > 0 and x < -10:
            return -1
        elif z == 0 or x == 0:
            return 0
    return x + y + z


def simple_function():
    return 42
