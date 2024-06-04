import { test, expect } from "@playwright/test";
import { createRoom } from "./utils";

test("join a room as a voter", async ({ page: roomPage, browser }) => {
  await roomPage.goto("/");

  const name = "best always sunny character";
  const characters = ["Charlie", "Frank", "Sweet Dee", "Dennis", "Mac"];
  await createRoom(roomPage, name, characters);

  const joinAddress = (await roomPage
    .getByTestId("voter-link")
    .textContent()) as string;

  const visitorContext = await browser.newContext();
  const visitorPage = await visitorContext.newPage();
  await visitorPage.goto(joinAddress);

  await expect(visitorPage).toHaveTitle("Join Room - ORDO");
  await expect(visitorPage.getByText(`JOIN THE "${name}" ROOM`)).toBeVisible();

  const joinRoom = visitorPage.getByTestId("join-room");
  await joinRoom.click();

  await visitorPage.waitForURL("/voters/*");

  await expect(roomPage.getByTestId("voter-count")).toHaveText("1");
  await expect(visitorPage.getByTestId("voter-count")).toHaveText("1");

  await visitorContext.close();
});
