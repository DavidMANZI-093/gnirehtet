/*
 * Copyright (C) 2017 Genymobile
 * Copyright (C) 2026 David MANZI
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package d3fault.gnirehtet;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.Arrays;
import org.junit.Assert;
import org.junit.Test;

public class TestIPPacketOutputStream {
  private ByteBuffer createMockPacket() {
    ByteBuffer buffer = ByteBuffer.allocate(32);
    writeMockPacketTo(buffer);
    buffer.flip();
    return buffer;
  }

  private void writeMockPacketTo(ByteBuffer buffer) {
    buffer.put((byte) ((4 << 4) | 5)); // versionAndIHL
    buffer.put((byte) 0); // ToS
    buffer.putShort((short) 32); // total length 20 + 8 + 4
    buffer.putInt(0); // IdFlagsFragmentOffset
    buffer.put((byte) 0); // TTL
    buffer.put((byte) 17); // protocol (UDP)
    buffer.putShort((short) 0); // checksum
    buffer.putInt(0x12345678); // source address
    buffer.putInt(0x42424242); // destination address

    buffer.putShort((short) 1234); // source port
    buffer.putShort((short) 5678); // destination port
    buffer.putShort((short) 12); // length
    buffer.putShort((short) 0); // checksum

    buffer.putInt(0x11223344); // payload
  }

  @Test
  public void testSimplePacket() throws IOException {
    ByteArrayOutputStream bos = new ByteArrayOutputStream();
    IPPacketOutputStream pos = new IPPacketOutputStream(bos);

    byte[] rawPacket = createMockPacket().array();

    pos.write(rawPacket, 0, 14);
    Assert.assertEquals("Partial packet should not be written", 0, bos.size());

    pos.write(rawPacket, 14, 14);
    Assert.assertEquals("Partial packet should not be written", 0, bos.size());

    pos.write(rawPacket, 28, 4);
    Assert.assertEquals("Complete packet should be written", 32, bos.size());

    byte[] result = bos.toByteArray();
    Assert.assertTrue("Resulting array must be identical", Arrays.equals(rawPacket, result));
  }

  @Test
  public void testMaximumIPv6Packet() throws IOException {
    ByteArrayOutputStream bos = new ByteArrayOutputStream();
    IPPacketOutputStream pos = new IPPacketOutputStream(bos);

    int packetLength = 40 + 65535;
    ByteBuffer buffer = ByteBuffer.allocate(packetLength);
    buffer.put((byte) (6 << 4)); // version
    buffer.put((byte) 0);
    buffer.putShort((short) 0); // flow label
    buffer.putShort((short) 65535); // payload length
    buffer.put((byte) 59); // no next header
    buffer.put((byte) 64); // hop limit
    buffer.position(packetLength);

    byte[] rawPacket = buffer.array();
    pos.write(rawPacket, 0, rawPacket.length);
    Assert.assertEquals("Maximum IPv6 packet should be written", packetLength, bos.size());
    Assert.assertTrue(
        "Resulting array must be identical", Arrays.equals(rawPacket, bos.toByteArray()));
  }

  @Test
  public void testReadPacketLength() {
    ByteBuffer ipv6 = ByteBuffer.allocate(40);
    ipv6.put((byte) (6 << 4));
    ipv6.put((byte) 0);
    ipv6.putShort((short) 0);
    ipv6.putShort((short) 65535);
    ipv6.flip();
    ipv6.limit(ipv6.capacity()); // the fixed header is present (padded with zeros)
    Assert.assertEquals(40 + 65535, IPPacketOutputStream.readPacketLength(ipv6, 6));

    ByteBuffer ipv4 = ByteBuffer.allocate(20);
    ipv4.put((byte) ((4 << 4) | 5));
    ipv4.put((byte) 0);
    ipv4.putShort((short) 20);
    ipv4.flip();
    Assert.assertEquals(20, IPPacketOutputStream.readPacketLength(ipv4, 4));

    ipv4.putShort(2, (short) 10); // smaller than an IPv4 header
    Assert.assertEquals(-1, IPPacketOutputStream.readPacketLength(ipv4, 4));
  }

  @Test
  public void testSeveralPacketsAtOnce() throws IOException {
    class CapturingOutputStream extends ByteArrayOutputStream {
      private int packetCount;

      @Override
      public void write(byte[] b, int off, int len) {
        super.write(b, off, len);
        ++packetCount;
      }
    }
    CapturingOutputStream cos = new CapturingOutputStream();
    IPPacketOutputStream pos = new IPPacketOutputStream(cos);

    ByteBuffer buffer = ByteBuffer.allocate(3 * 32);
    for (int i = 0; i < 3; ++i) {
      writeMockPacketTo(buffer);
    }
    byte[] rawPackets = buffer.array();

    pos.write(rawPackets, 0, 70); // 2 packets + 6 bytes
    Assert.assertEquals("Exactly 2 packets should have been written", 64, cos.size());
    Assert.assertEquals("Packets should be written individually to the target", 2, cos.packetCount);

    pos.write(rawPackets, 70, 26);
    Assert.assertEquals("Exactly 3 packets should have been written", 96, cos.size());
    Assert.assertEquals("Packets should be written individually to the target", 3, cos.packetCount);
  }
}
