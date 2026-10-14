#!/usr/bin/env python3
"""Generate offline wallet and order-ID vectors with the official CLOB SDK.

Reference: Polymarket/py-clob-client-v2 commit
292c11005d748c21342a9457d7c0ac89afc2e3f2, package version 1.1.0.
The public dummy key below holds no assets. No client, network request, or
order submission is constructed. Install the reference in a separate env.
"""

import argparse
import json
from importlib.metadata import version
from pathlib import Path

from py_clob_client_v2.config import get_contract_config
from py_clob_client_v2.order_utils.exchange_order_builder_v1 import ExchangeOrderBuilderV1
from py_clob_client_v2.order_utils.exchange_order_builder_v2 import ExchangeOrderBuilderV2
from py_clob_client_v2.order_utils.model.order_data_v1 import OrderDataV1
from py_clob_client_v2.order_utils.model.order_data_v2 import OrderDataV2
from py_clob_client_v2.signer import Signer


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=Path("tests/fixtures/clob_wallet_vectors.json"))
    args = parser.parse_args()
    if version("py_clob_client_v2") != "1.1.0":
        raise SystemExit("Use the documented reference SDK version 1.1.0")
    signer = Signer("0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", 137)
    config = get_contract_config(137)
    cases = []
    for protocol in (1, 2, 3):
        for signature_type in range(3 if protocol == 1 else 4):
            funder = signer.address() if signature_type == 0 else "0x1111111111111111111111111111111111111111"
            order_signer = funder if signature_type == 3 else signer.address()
            for neg_risk in ((False,) if protocol == 3 else (False, True)):
                for side in (0, 1):
                    maker, taker = (2_677_500, 5_250_000) if side == 0 else (5_250_000, 2_677_500)
                    fields = dict(maker=funder, signer=order_signer, tokenId="123456789",
                                  makerAmount=str(maker), takerAmount=str(taker), side=side,
                                  signatureType=signature_type)
                    if protocol == 1:
                        exchange = config.neg_risk_exchange if neg_risk else config.exchange
                        builder = ExchangeOrderBuilderV1(exchange, 137, signer, generate_salt=lambda: 7)
                        data = OrderDataV1(**fields, taker="0x" + "00" * 20,
                                           feeRateBps="25", nonce="11", expiration="9999")
                    else:
                        exchange = (config.exchange_v3 if protocol == 3 else
                                    config.neg_risk_exchange_v2 if neg_risk else config.exchange_v2)
                        builder = ExchangeOrderBuilderV2(exchange, 137, signer, generate_salt=lambda: 7,
                                                         domain_version=str(protocol))
                        data = OrderDataV2(**fields, timestamp="1717171717000")
                    order = builder.build_signed_order(data)
                    typed_data = builder.build_order_typed_data(order)
                    cases.append({"protocol": protocol, "signature_type": signature_type,
                                  "neg_risk": neg_risk, "side": side, "maker": str(maker),
                                  "taker": str(taker), "funder": funder, "signer": order_signer,
                                  "exchange": exchange, "order_hash": builder.build_order_hash(typed_data).removeprefix("0x"),
                                  "signature": order.signature.removeprefix("0x")})
    fixture = {"reference_repository": "https://github.com/Polymarket/py-clob-client-v2",
               "reference_commit": "292c11005d748c21342a9457d7c0ac89afc2e3f2",
               "sdk_version": version("py_clob_client_v2"), "signer_address": signer.address(),
               "cases": cases}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(fixture, indent=2, sort_keys=True) + "\n")
    print(f"Generated {len(cases)} wallet signing and canonical order-ID cases")


if __name__ == "__main__":
    main()
