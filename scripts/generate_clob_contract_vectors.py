#!/usr/bin/env python3
"""Generate offline CLOB contract fixtures using the official Python SDK.

Reference: Polymarket/py-clob-client-v2 commit
292c11005d748c21342a9457d7c0ac89afc2e3f2 (version 1.1.0).
Install that reference in a separate Python 3.10+ environment. This script
uses a public dummy key, creates no client connection and sends no orders.
"""

import argparse
import json
from importlib.metadata import version
from pathlib import Path

from py_clob_client_v2.config import get_contract_config
from py_clob_client_v2.order_builder.builder import OrderBuilder, ROUNDING_CONFIG
from py_clob_client_v2.order_utils.exchange_order_builder_v1 import ExchangeOrderBuilderV1
from py_clob_client_v2.order_utils.exchange_order_builder_v2 import ExchangeOrderBuilderV2
from py_clob_client_v2.order_utils.model.order_data_v1 import OrderDataV1, order_to_json_v1
from py_clob_client_v2.order_utils.model.order_data_v2 import OrderDataV2, order_to_json_v2
from py_clob_client_v2.signer import Signer
from py_clob_client_v2.signing.eip712 import sign_clob_auth_message
from py_clob_client_v2.signing.hmac import build_hmac_signature


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=Path("tests/fixtures/clob_contract_vectors.json"))
    args = parser.parse_args()
    if version("py_clob_client_v2") != "1.1.0":
        raise SystemExit("Use the documented reference SDK version 1.1.0")
    signer = Signer("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", 137)
    config = get_contract_config(137)
    builder = OrderBuilder(signer)
    sizing = []
    orders = []
    for tick, price in [("0.1", 0.3), ("0.01", 0.51), ("0.001", 0.513),
                        ("0.0001", 0.5137), ("0.005", 0.515), ("0.0025", 0.5125)]:
        for side in ["BUY", "SELL"]:
            side_number, maker, taker = builder.get_order_amounts(side, 5.257, price, ROUNDING_CONFIG[tick])
            sizing.append({"tick": float(tick), "price": price, "size": 5.257,
                           "side": side, "maker": str(maker), "taker": str(taker)})
            for protocol in [1, 2]:
                for neg_risk in [False, True]:
                    fields = dict(maker=signer.address(), signer=signer.address(), tokenId="123456789",
                                  makerAmount=str(maker), takerAmount=str(taker), side=side_number)
                    if protocol == 1:
                        address = config.neg_risk_exchange if neg_risk else config.exchange
                        data = OrderDataV1(**fields, taker="0x" + "00" * 20,
                                           feeRateBps="25", nonce="11", expiration="9999")
                        order = ExchangeOrderBuilderV1(address, 137, signer, generate_salt=lambda: 7).build_signed_order(data)
                        wire = order_to_json_v1(order, "api-key-123", "GTC")
                    else:
                        address = config.neg_risk_exchange_v2 if neg_risk else config.exchange_v2
                        data = OrderDataV2(**fields, timestamp="1717171717000")
                        order = ExchangeOrderBuilderV2(address, 137, signer, generate_salt=lambda: 7).build_signed_order(data)
                        wire = order_to_json_v2(order, "api-key-123", "GTC")
                    orders.append({"protocol": protocol, "neg_risk": neg_risk,
                                   "side": int(side_number), "maker": str(maker), "taker": str(taker),
                                   "signature": order.signature.removeprefix("0x"),
                                   "wire_keys": sorted(wire["order"])})
    fixture = {
        "reference_repository": "https://github.com/Polymarket/py-clob-client-v2",
        "reference_commit": "292c11005d748c21342a9457d7c0ac89afc2e3f2",
        "sdk_version": version("py_clob_client_v2"),
        "l1_signature": sign_clob_auth_message(signer, 1717171717, 42).removeprefix("0x"),
        "hmac_secret": "cHVibGljLWZpeHR1cmUtc2VjcmV0",
        "hmac_body": '{"market":"mkt-1","size":2}',
        "hmac_signature": build_hmac_signature("cHVibGljLWZpeHR1cmUtc2VjcmV0", 1717171717, "POST", "/orders", '{"market":"mkt-1","size":2}'),
        "sizing": sizing, "orders": orders,
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(fixture, indent=2, sort_keys=True) + "\n")
    print(f"Generated {len(sizing)} sizing cases and {len(orders)} signing cases")


if __name__ == "__main__":
    main()
